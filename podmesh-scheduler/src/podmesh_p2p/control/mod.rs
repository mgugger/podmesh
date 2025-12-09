use libp2p::kad::RecordKey;
use libp2p::{PeerId, Swarm, gossipsub};
use log::{info, warn};
use once_cell::sync::Lazy;
use std::collections::HashMap as StdHashMap;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::podmesh_p2p::behaviour::MyBehaviour;
use libp2p::kad::QueryId;

mod query_capacity;
mod send_apply_request;
mod send_delete_request;

pub use query_capacity::handle_query_capacity_with_payload;
pub use send_apply_request::handle_send_apply_request;
pub use send_delete_request::handle_send_delete_request;

/// Handle incoming control messages from other parts of the host (REST handlers)
pub async fn handle_control_message(
    msg: Libp2pControl,
    swarm: &mut Swarm<MyBehaviour>,
    topic: &gossipsub::IdentTopic,
    pending_queries: &mut StdHashMap<String, Vec<mpsc::UnboundedSender<String>>>,
) {
    match msg {
        Libp2pControl::QueryCapacityWithPayload {
            request_id,
            reply_tx,
            payload,
        } => {
            handle_query_capacity_with_payload(
                request_id,
                reply_tx,
                payload,
                swarm,
                topic,
                pending_queries,
            )
            .await;
        }
        Libp2pControl::SendApplyRequest {
            peer_id,
            manifest,
            reply_tx,
        } => {
            handle_send_apply_request(peer_id, manifest, reply_tx, swarm).await;
        }
        Libp2pControl::SendDeleteRequest {
            peer_id,
            delete_request,
            reply_tx,
        } => {
            handle_send_delete_request(peer_id, delete_request, reply_tx, swarm).await;
        }

        Libp2pControl::FindManifestHolders {
            manifest_id,
            reply_tx,
        } => {
            // Use Kademlia providers API by issuing a get_providers for the provider key
            let record_key = RecordKey::new(&format!("provider:{}", manifest_id));
            info!(
                "DHT: attempting get_providers for key=provider:{}",
                manifest_id
            );
            let query_id = swarm.behaviour_mut().kademlia.get_providers(record_key);
            // Register pending sender so the kademlia_event handler can reply when results arrive
            insert_pending_providers_query(query_id, reply_tx);
            info!(
                "DHT: initiated find_providers for manifest (query_id={:?})",
                query_id
            );
        }

        Libp2pControl::BootstrapDht { reply_tx } => {
            let _ = swarm.behaviour_mut().kademlia.bootstrap();
            let _ = reply_tx.send(Ok(()));
        }
        Libp2pControl::AnnounceProvider {
            cid,
            ttl_ms,
            reply_tx,
        } => {
            log::warn!(
                "libp2p: processing AnnounceProvider for cid={} ttl_ms={}",
                cid,
                ttl_ms
            );
            // For local tests, announce multiple times with short intervals to improve discovery
            let mut announce_success = false;
            for attempt in 1..=3 {
                match announce_provider_direct(swarm, cid.clone(), ttl_ms) {
                    Ok(()) => {
                        announce_success = true;
                        if attempt > 1 {
                            log::info!(
                                "libp2p: AnnounceProvider succeeded for cid={} on attempt {}",
                                cid,
                                attempt
                            );
                        }
                        break;
                    }
                    Err(e) => {
                        log::warn!(
                            "libp2p: AnnounceProvider attempt {} failed for cid={}: {}",
                            attempt,
                            cid,
                            e
                        );
                        if attempt < 3 {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                }
            }

            if announce_success {
                // schedule next renewal at half the TTL (or 1s minimum) with more frequent updates for local tests
                let ttl = if ttl_ms == 0 { 0 } else { ttl_ms };
                let next = if ttl == 0 {
                    Instant::now() + Duration::from_secs(3600) // effectively never
                } else {
                    // More frequent renewal for local tests - every 2 seconds minimum
                    Instant::now() + Duration::from_millis(std::cmp::max(2000, ttl / 3))
                };
                let mut map = ACTIVE_ANNOUNCES
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                map.insert(
                    cid,
                    AnnounceEntry {
                        ttl_ms: ttl_ms,
                        next_due: next,
                    },
                );
                let _ = reply_tx.send(Ok(()));
            } else {
                let _ = reply_tx.send(Err("All provider announcement attempts failed".to_string()));
            }
        }
        Libp2pControl::GetDhtPeers { reply_tx } => {
            // Get DHT peer information for debugging
            let connected_peers: Vec<_> = swarm.connected_peers().cloned().collect();
            let local_peer_id = *swarm.local_peer_id();

            let dht_info = serde_json::json!({
                "local_peer_id": local_peer_id.to_string(),
                "connected_peers": connected_peers.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                "connected_count": connected_peers.len()
            });

            let _ = reply_tx.send(Ok(dht_info));
        }
        Libp2pControl::WithdrawProvider { cid, reply_tx } => {
            // Remove from active announces and issue a withdraw (expiry=now)
            {
                let mut map = ACTIVE_ANNOUNCES
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                map.remove(&cid);
            }
            match announce_provider_direct(swarm, cid.clone(), 0) {
                Ok(()) => {
                    let _ = reply_tx.send(Ok(()));
                }
                Err(e) => {
                    let _ = reply_tx.send(Err(e));
                }
            }
        }
        Libp2pControl::GetConnectedPeers { reply_tx } => {
            // Get list of currently connected peers
            let connected_peers: Vec<libp2p::PeerId> = swarm.connected_peers().cloned().collect();
            let _ = reply_tx.send(connected_peers);
        }
        Libp2pControl::GetLocalPeerId { reply_tx } => {
            // Get the local peer ID
            let local_peer_id = *swarm.local_peer_id();
            let _ = reply_tx.send(local_peer_id);
        }
        Libp2pControl::GetPeerPublicKey { peer_id, reply_tx } => {
            // Get a peer's public key in base64 format
            // For now, return the peer_id as a placeholder - in production,
            // this should query the peer's identity or stored keys
            // The public key should be obtained during peer connection/handshake
            let _ = reply_tx.send(peer_id);
        }
        Libp2pControl::GetProviderRecord {
            manifest_id,
            reply_tx,
        } => {
            // Get the provider announcement record from DHT
            let record_key = RecordKey::new(&format!("provider:{}", manifest_id));
            info!(
                "DHT: attempting get_record for provider:{}",
                manifest_id
            );
            let query_id = swarm.behaviour_mut().kademlia.get_record(record_key);
            // Register pending sender so the kademlia_event handler can reply when results arrive
            insert_pending_provider_record_query(query_id, reply_tx);
            info!(
                "DHT: initiated get_record for provider announcement (query_id={:?})",
                manifest_id
            );
        }
        Libp2pControl::FetchPeerKemPubkey { peer_id, reply_tx } => {
            // Fetch KEM public key from peer via handshake request
            // We send a handshake request and the response will contain the peer's KEM pubkey
            info!("Fetching KEM pubkey from peer {}", peer_id);
            
            // Register the pending request so we can extract KEM pubkey from response
            insert_pending_kem_pubkey_request(peer_id, reply_tx);
            
            // Send a handshake request to the peer
            let request = match p2p::handshake::build_handshake_request_for_kem_fetch(swarm.local_peer_id()) {
                Ok(req) => req,
                Err(e) => {
                    warn!("Failed to build handshake request for KEM fetch: {}", e);
                    return;
                }
            };
            
            swarm
                .behaviour_mut()
                .handshake_rr
                .send_request(&peer_id, request);
        }
    }
}

/// Announce a provider record directly using the swarm's Kademlia behaviour.
/// This helper centralizes the key/record format used for provider announcements.
pub fn announce_provider_direct(
    swarm: &mut Swarm<MyBehaviour>,
    cid: String,
    _ttl_ms: u64,
) -> Result<(), String> {
    // Use Kademlia's provider mechanism instead of records
    let record_key = RecordKey::new(&cid);

    // Check if we have sufficient DHT connectivity before announcing
    let connected_peers: Vec<_> = swarm.connected_peers().cloned().collect();
    if connected_peers.is_empty() {
        return Err("No connected peers for DHT announcement".into());
    }

    match swarm.behaviour_mut().kademlia.start_providing(record_key) {
        Ok(query_id) => {
            log::info!(
                "DHT: announced provider for cid={} (query_id={:?}) with {} connected peers",
                cid,
                query_id,
                connected_peers.len()
            );
            Ok(())
        }
        Err(e) => Err(format!("kademlia start_providing failed: {:?}", e)),
    }
}

/// Entry for active announce tracking
struct AnnounceEntry {
    ttl_ms: u64,
    next_due: Instant,
}

/// Map of active announces: cid -> entry
static ACTIVE_ANNOUNCES: Lazy<Mutex<HashMap<String, AnnounceEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Queue for control messages enqueued from inside the libp2p behaviours.
static ENQUEUED_CONTROLS: Lazy<Mutex<std::collections::VecDeque<Libp2pControl>>> =
    Lazy::new(|| Mutex::new(std::collections::VecDeque::new()));

/// Pending provider lookup queries: QueryId string -> reply sender (Vec<PeerId>)
static PENDING_PROVIDERS_QUERIES: Lazy<
    Mutex<std::collections::HashMap<String, mpsc::UnboundedSender<Vec<libp2p::PeerId>>>>,
> = Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

/// Pending provider record queries: QueryId string -> reply sender (Option<HashMap<String, String>>)
static PENDING_PROVIDER_RECORD_QUERIES: Lazy<
    Mutex<std::collections::HashMap<String, mpsc::UnboundedSender<Option<HashMap<String, String>>>>>,
> = Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

/// Pending KEM pubkey fetch requests: PeerId string -> reply sender (Option<String>)
static PENDING_KEM_PUBKEY_REQUESTS: Lazy<
    Mutex<std::collections::HashMap<String, mpsc::UnboundedSender<Option<String>>>>,
> = Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

/// Insert a pending providers query sender for the given QueryId
pub fn insert_pending_providers_query(
    id: QueryId,
    sender: mpsc::UnboundedSender<Vec<libp2p::PeerId>>,
) {
    let key = format!("{:?}", id);
    let mut map = PENDING_PROVIDERS_QUERIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.insert(key, sender);
}

/// Take and remove a pending providers query sender by QueryId
pub fn take_pending_providers_query(
    id: &QueryId,
) -> Option<mpsc::UnboundedSender<Vec<libp2p::PeerId>>> {
    let key = format!("{:?}", id);
    let mut map = PENDING_PROVIDERS_QUERIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.remove(&key)
}

/// Insert a pending provider record query sender for the given QueryId
pub fn insert_pending_provider_record_query(
    id: QueryId,
    sender: mpsc::UnboundedSender<Option<HashMap<String, String>>>,
) {
    let key = format!("{:?}", id);
    let mut map = PENDING_PROVIDER_RECORD_QUERIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.insert(key, sender);
}

/// Take and remove a pending provider record query sender by QueryId
pub fn take_pending_provider_record_query(
    id: &QueryId,
) -> Option<mpsc::UnboundedSender<Option<HashMap<String, String>>>> {
    let key = format!("{:?}", id);
    let mut map = PENDING_PROVIDER_RECORD_QUERIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.remove(&key)
}

/// Insert a pending KEM pubkey request sender for the given PeerId
pub fn insert_pending_kem_pubkey_request(
    peer_id: libp2p::PeerId,
    sender: mpsc::UnboundedSender<Option<String>>,
) {
    let key = peer_id.to_string();
    let mut map = PENDING_KEM_PUBKEY_REQUESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.insert(key, sender);
}

/// Take and remove a pending KEM pubkey request sender by PeerId
pub fn take_pending_kem_pubkey_request(
    peer_id: &libp2p::PeerId,
) -> Option<mpsc::UnboundedSender<Option<String>>> {
    let key = peer_id.to_string();
    let mut map = PENDING_KEM_PUBKEY_REQUESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.remove(&key)
}

/// Enqueue a control message from inside a behaviour handler. This is synchronous and cheap.
pub fn enqueue_control(msg: Libp2pControl) {
    let mut q = ENQUEUED_CONTROLS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    q.push_back(msg);
}

/// Drain and handle enqueued controls. Should be called from the libp2p task (async context).
pub async fn drain_enqueued_controls(
    swarm: &mut Swarm<MyBehaviour>,
    topic: &gossipsub::IdentTopic,
    pending_queries: &mut StdHashMap<String, Vec<mpsc::UnboundedSender<String>>>,
) {
    // Move queued items into a local vector to minimize lock time
    let mut items = Vec::new();
    {
        let mut q = ENQUEUED_CONTROLS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while let Some(it) = q.pop_front() {
            items.push(it);
        }
    }

    for msg in items {
        handle_control_message(msg, swarm, topic, pending_queries).await;
    }
}

/// Renew any announces that are due. Intended to be called periodically from the libp2p loop.
pub fn renew_due_providers(swarm: &mut Swarm<MyBehaviour>) {
    let now = Instant::now();
    let mut map = ACTIVE_ANNOUNCES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for (cid, entry) in map.iter_mut() {
        if entry.ttl_ms == 0 {
            continue;
        }
        if entry.next_due <= now {
            let _ = announce_provider_direct(swarm, cid.clone(), entry.ttl_ms);
            // More frequent renewal for local tests - every 2 seconds minimum
            entry.next_due =
                Instant::now() + Duration::from_millis(std::cmp::max(2000, entry.ttl_ms / 3));
        }
    }
}

/// Withdraw all announces (used on shutdown)
pub fn withdraw_all_providers(swarm: &mut Swarm<MyBehaviour>) {
    let keys: Vec<String> = {
        let mut m = ACTIVE_ANNOUNCES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        m.drain().map(|(k, _)| k).collect()
    };
    for cid in keys {
        let _ = announce_provider_direct(swarm, cid.clone(), 0);
        info!("DHT: withdrew provider for cid={}", cid);
    }
}

/// Return a snapshot of active announce CIDs
pub fn list_active_announces() -> Vec<String> {
    let map = ACTIVE_ANNOUNCES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.keys().cloned().collect()
}

/// Control messages sent from the rest API or other parts of the host to the libp2p task.
#[derive(Debug)]
pub enum Libp2pControl {
    QueryCapacityWithPayload {
        request_id: String,
        reply_tx: mpsc::UnboundedSender<String>,
        payload: Vec<u8>,
    },
    SendApplyRequest {
        peer_id: PeerId,
        /// Postcard ApplyRequest bytes
        manifest: Vec<u8>,
        reply_tx: mpsc::UnboundedSender<Result<String, String>>,
    },
    SendDeleteRequest {
        peer_id: PeerId,
        /// Postcard DeleteRequest bytes
        delete_request: Vec<u8>,
        reply_tx: mpsc::UnboundedSender<Result<String, String>>,
    },

    /// Find peers that hold shares for a given manifest id using DHT providers
    FindManifestHolders {
        manifest_id: String,
        reply_tx: mpsc::UnboundedSender<Vec<libp2p::PeerId>>,
    },
    /// Bootstrap the DHT by connecting to known peers
    BootstrapDht {
        reply_tx: mpsc::UnboundedSender<Result<(), String>>,
    },
    /// Announce that this node holds data for the given CID (provider-style record)
    AnnounceProvider {
        cid: String,
        ttl_ms: u64,
        reply_tx: mpsc::UnboundedSender<Result<(), String>>,
    },
    /// Withdraw a previously announced provider
    WithdrawProvider {
        cid: String,
        reply_tx: mpsc::UnboundedSender<Result<(), String>>,
    },
    /// Get DHT peer information for debugging
    GetDhtPeers {
        reply_tx: mpsc::UnboundedSender<Result<serde_json::Value, String>>,
    },
    /// Get list of currently connected peers
    GetConnectedPeers {
        reply_tx: mpsc::UnboundedSender<Vec<libp2p::PeerId>>,
    },
    /// Get the local peer ID
    GetLocalPeerId {
        reply_tx: mpsc::UnboundedSender<libp2p::PeerId>,
    },
    /// Get a peer's public key in base64 format
    GetPeerPublicKey {
        peer_id: String,
        reply_tx: mpsc::UnboundedSender<String>,
    },
    /// Get the provider announcement record from DHT (includes metadata with KEM key)
    GetProviderRecord {
        manifest_id: String,
        reply_tx: mpsc::UnboundedSender<Option<HashMap<String, String>>>,
    },
    /// Fetch KEM public key from a peer via handshake protocol
    FetchPeerKemPubkey {
        peer_id: libp2p::PeerId,
        reply_tx: mpsc::UnboundedSender<Option<String>>,
    },
}
