//! Phase 5: coordinator-side worker discovery and WorkloadDispatch.
//!
//! The elected coordinator for a manifest:
//! 1. Broadcasts a `CapabilityQuery(role=worker, required_capabilities=...)` — non-blocking.
//! 2. When a `ResourceReply(ok=true)` arrives for the query (in handle_scheduler_response),
//!    `notify_worker_dispatch` is called, which enqueues a `SendWorkloadDispatch` control.
//! 3. `handle_send_workload_dispatch` sends a `WorkloadDispatch` to the winning worker.

use libp2p::{Swarm, gossipsub};
use log::{error, info, warn};
use once_cell::sync::Lazy;
use std::collections::HashMap as StdHashMap;
use std::sync::Mutex;
use tokio::sync::mpsc;

use protocol::machine::{SealedSpec, WorkloadDispatch, build_capability_query};
use crate::podmesh_p2p::behaviour::MyBehaviour;
use crate::podmesh_p2p::utils;

// ---------------------------------------------------------------------------
// Pending worker dispatch state: query_id → (SealedSpec, custodian_peers, required_caps)
// ---------------------------------------------------------------------------

struct PendingWorkerDispatch {
    sealed_spec: SealedSpec,
    all_custodian_peers: Vec<String>,
    required_capabilities: Vec<String>,
    /// Target number of workers to dispatch to.
    replica_count: u8,
    /// How many workers have been dispatched so far.
    dispatched_count: u8,
    /// Track workers already dispatched to avoid duplicates.
    dispatched_workers: Vec<String>,
    /// The coordinator's own peer_id — used to route enqueue_control to the right swarm.
    coordinator_peer_id: String,
}

static PENDING_WORKER_DISPATCHES: Lazy<Mutex<StdHashMap<String, PendingWorkerDispatch>>> =
    Lazy::new(|| Mutex::new(StdHashMap::new()));

/// Tracks outbound WorkloadDispatch request IDs so we can retry on OutboundFailure.
/// key: "{coordinator_peer_id}:{request_id}", value: (coordinator_peer_id, worker_peer_id_str, worker_kem_pub_b64, SealedSpec, custodian_peers, required_caps)
#[allow(clippy::type_complexity)]
static PENDING_DISPATCH_SENDS: Lazy<Mutex<StdHashMap<String, (String, String, String, SealedSpec, Vec<String>, Vec<String>)>>> =
    Lazy::new(|| Mutex::new(StdHashMap::new()));

/// Called when a `scheduler_rr` OutboundFailure occurs.  If the request was a WorkloadDispatch,
/// re-enqueue `SendWorkloadDispatch` so the coordinator retries delivery.
pub fn handle_workload_dispatch_outbound_failure(coordinator_peer_id: &str, request_id_str: &str) {
    let key = format!("{}:{}", coordinator_peer_id, request_id_str);
    let entry = {
        let mut map = PENDING_DISPATCH_SENDS.lock().unwrap_or_else(|p| p.into_inner());
        map.remove(&key)
    };
    if let Some((coordinator_peer_id, worker_peer_id_str, worker_kem_pub_b64, sealed_spec, custodian_peers, caps)) = entry {
        warn!(
            "dispatch_to_worker: WorkloadDispatch OutboundFailure for manifest_id={} to worker={}, retrying",
            sealed_spec.manifest_id, worker_peer_id_str
        );
        crate::podmesh_p2p::control::enqueue_control(
            &coordinator_peer_id,
            crate::podmesh_p2p::control::Libp2pControl::SendWorkloadDispatch {
                worker_peer_id_str,
                worker_kem_pub_b64,
                sealed_spec,
                all_custodian_peers: custodian_peers,
                required_capabilities: caps,
            },
        );
    }
}

/// Remove a completed WorkloadDispatch send from the pending map (success case).
pub fn workload_dispatch_send_completed(coordinator_peer_id: &str, request_id_str: &str) {
    let key = format!("{}:{}", coordinator_peer_id, request_id_str);
    PENDING_DISPATCH_SENDS.lock().unwrap_or_else(|p| p.into_inner()).remove(&key);
}

/// Called from `handle_scheduler_response` when `ResourceReply.ok=true` arrives for a
/// `worker:` query.  Enqueues a `SendWorkloadDispatch` control for eligible workers up to
/// `replica_count`.
pub fn notify_worker_dispatch(query_id: &str, worker_peer_id_str: &str, worker_kem_pub_b64: &str) {
    let mut map = PENDING_WORKER_DISPATCHES
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    if let Some(entry) = map.get_mut(query_id) {
        // Skip if we've already dispatched to this worker.
        if entry.dispatched_workers.contains(&worker_peer_id_str.to_string()) {
            return;
        }
        // Skip if we've already reached the replica target.
        if entry.dispatched_count >= entry.replica_count {
            return;
        }

        entry.dispatched_count += 1;
        entry.dispatched_workers.push(worker_peer_id_str.to_string());

        info!(
            "dispatch_to_worker: notify_worker_dispatch: enqueuing SendWorkloadDispatch for manifest_id={} to worker={} (replica {}/{})",
            entry.sealed_spec.manifest_id, worker_peer_id_str, entry.dispatched_count, entry.replica_count
        );

        let worker_peer_id_str = worker_peer_id_str.to_string();
        let worker_kem_pub_b64 = worker_kem_pub_b64.to_string();
        let sealed_spec = entry.sealed_spec.clone();
        let all_custodian_peers = entry.all_custodian_peers.clone();
        let required_capabilities = entry.required_capabilities.clone();
        let coordinator_peer_id = entry.coordinator_peer_id.clone();

        drop(map); // release the lock before enqueuing

        crate::podmesh_p2p::control::enqueue_control(
            &coordinator_peer_id,
            crate::podmesh_p2p::control::Libp2pControl::SendWorkloadDispatch {
                worker_peer_id_str,
                worker_kem_pub_b64,
                sealed_spec,
                all_custodian_peers,
                required_capabilities,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Non-blocking dispatch: broadcast CapabilityQuery and register pending state
// ---------------------------------------------------------------------------

pub async fn handle_dispatch_to_worker(
    sealed_spec: SealedSpec,
    all_custodian_peers: Vec<String>,
    required_capabilities: Vec<String>,
    replica_count: u8,
    swarm: &mut Swarm<MyBehaviour>,
    _topic: &gossipsub::IdentTopic,
    pending_queries: &mut StdHashMap<String, Vec<mpsc::UnboundedSender<String>>>,
) {
    let manifest_id = sealed_spec.manifest_id.clone();

    info!(
        "dispatch_to_worker: manifest_id={} discovering workers (caps={:?})",
        manifest_id, required_capabilities
    );

    // --- Broadcast CapabilityQuery for a worker ---
    let query_id = format!("worker:{}:{}", manifest_id, uuid::Uuid::new_v4());
    let nonce = utils::make_nonce(Some("dtw"));
    let initiator_pubkey = crypto::ensure_keypair_on_disk()
        .ok()
        .map(|(pk, _)| crypto::b64_encode(&pk))
        .unwrap_or_default();

    let caps_str: Vec<&str> = if required_capabilities.is_empty() {
        vec!["default"]
    } else {
        required_capabilities.iter().map(|s| s.as_str()).collect()
    };

    let cap_query_bytes = build_capability_query(
        &query_id,
        &nonce,
        &caps_str,
        "worker",
        &initiator_pubkey,
    );

    // Register the pending worker dispatch so we can act when a ResourceReply arrives.
    let local_peer_id_str = swarm.local_peer_id().to_string();
    {
        let mut map = PENDING_WORKER_DISPATCHES
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        map.insert(query_id.clone(), PendingWorkerDispatch {
            sealed_spec,
            all_custodian_peers,
            required_capabilities,
            replica_count: replica_count.max(1),
            dispatched_count: 0,
            dispatched_workers: Vec::new(),
            coordinator_peer_id: local_peer_id_str.clone(),
        });
    }

    // Also register a dummy pending_queries entry so handle_scheduler_response routes
    // the ResourceReply.ok=true to notify_capacity_observers → our worker dispatch hook.
    let (tx, _rx) = mpsc::unbounded_channel::<String>();
    pending_queries.entry(query_id.clone()).or_default().push(tx);

    match utils::broadcast_signed_request_to_peers(swarm, &cap_query_bytes, "capability_query") {
        Ok(sent) => info!("dispatch_to_worker: broadcast {} to {} peers", query_id, sent),
        Err(e) => {
            warn!("dispatch_to_worker: broadcast failed: {:?}", e);
        }
    }

    // Also evaluate the local node itself as a candidate worker (the broadcast only goes to
    // remote peers; the coordinator is never included in its own capability query responses).
    let local_peer_id = swarm.local_peer_id();
    if !crate::podmesh_p2p::is_scheduling_disabled_for(local_peer_id) {
        let local_kem_pub_b64 = crypto::ensure_kem_keypair_on_disk()
            .ok()
            .map(|(pub_bytes, _)| crypto::b64_encode(&pub_bytes))
            .unwrap_or_default();
        notify_worker_dispatch(&query_id, &local_peer_id.to_string(), &local_kem_pub_b64);
    }

    // Spawn a retry task: if fewer than replica_count workers have been dispatched within
    // the timeout, re-enqueue DispatchToWorker (handles transient DialFailures).
    {
        let query_id_clone = query_id.clone();
        let coordinator_peer_id_retry = local_peer_id_str.clone();
        let sealed_spec_clone = {
            let map = PENDING_WORKER_DISPATCHES.lock().unwrap_or_else(|p| p.into_inner());
            map.get(&query_id_clone).map(|e| (e.sealed_spec.clone(), e.all_custodian_peers.clone(), e.required_capabilities.clone(), e.replica_count, e.dispatched_count))
        };
        if let Some((sealed_spec_retry, custodians_retry, caps_retry, _replica_count_retry, _)) = sealed_spec_clone {
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                let (dispatched, target) = {
                    let map = PENDING_WORKER_DISPATCHES.lock().unwrap_or_else(|p| p.into_inner());
                    map.get(&query_id_clone)
                        .map(|e| (e.dispatched_count, e.replica_count))
                        .unwrap_or((1, 1)) // default: assume done
                };
                if dispatched < target {
                    info!(
                        "dispatch_to_worker: only {}/{} replicas dispatched for query={}, retrying DispatchToWorker for manifest_id={}",
                        dispatched, target, query_id_clone, sealed_spec_retry.manifest_id
                    );
                    // Remove the stale entry so a fresh query_id is used on retry.
                    PENDING_WORKER_DISPATCHES.lock().unwrap_or_else(|p| p.into_inner()).remove(&query_id_clone);
                    // Re-enqueue with remaining replica count.
                    let remaining = target - dispatched;
                    crate::podmesh_p2p::control::enqueue_control(
                        &coordinator_peer_id_retry,
                        crate::podmesh_p2p::control::Libp2pControl::DispatchToWorker {
                            sealed_spec: sealed_spec_retry,
                            all_custodian_peers: custodians_retry,
                            required_capabilities: caps_retry,
                            replica_count: remaining,
                        },
                    );
                }
            });
        }
    }

    // Non-blocking: return immediately. When a worker replies (ResourceReply.ok=true),
    // handle_scheduler_response → notify_capacity_observers → our worker_dispatch hook fires.
}

// ---------------------------------------------------------------------------
// Send WorkloadDispatch to the selected worker
// ---------------------------------------------------------------------------

pub fn handle_send_workload_dispatch(
    worker_peer_id_str: String,
    worker_kem_pub_b64: String,
    sealed_spec: SealedSpec,
    all_custodian_peers: Vec<String>,
    _required_capabilities: Vec<String>,
    swarm: &mut Swarm<MyBehaviour>,
) {
    let manifest_id = sealed_spec.manifest_id.clone();

    let worker_peer_id = match worker_peer_id_str.parse::<libp2p::PeerId>() {
        Ok(id) => id,
        Err(e) => {
            error!("dispatch_to_worker: invalid worker peer_id '{}': {}", worker_peer_id_str, e);
            return;
        }
    };

    // Decode the worker's KEM public key so we can re-wrap shares for it.
    let worker_kem_pub = match crypto::b64_decode(&worker_kem_pub_b64) {
        Ok(b) => b,
        Err(e) => {
            error!("dispatch_to_worker: invalid worker KEM pubkey for {}: {}", manifest_id, e);
            return;
        }
    };

    let local_peer_id = swarm.local_peer_id().to_string();

    // Collect the coordinator's own share from the local store.
    let mut worker_wrapped_shares = collect_local_shares_for_worker(
        &manifest_id, &worker_kem_pub, &local_peer_id, &all_custodian_peers,
    );

    // Also request shares from all other custodians (the coordinator is connected to them).
    // We issue the requests synchronously here, then collect replies in a spawned task.
    let threshold = sealed_spec.kfrag_threshold as usize;

    let coordinator_sig = {
        let sealed_bytes = sealed_spec.to_bytes();
        crypto::ensure_keypair_on_disk()
            .ok()
            .and_then(|(_, sk)| crypto::sign_data_with_key(&sk, &sealed_bytes).ok())
            .map(|s| crypto::b64_encode(&s))
            .unwrap_or_default()
    };

    let assigned_at_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let assignment_token = {
        let mut msg = Vec::new();
        msg.extend_from_slice(manifest_id.as_bytes());
        msg.extend_from_slice(worker_peer_id_str.as_bytes());
        msg.extend_from_slice(&assigned_at_secs.to_be_bytes());
        crypto::ensure_keypair_on_disk()
            .ok()
            .and_then(|(_, sk)| crypto::sign_data_with_key(&sk, &msg).ok())
            .map(|s| crypto::b64_encode(&s))
            .unwrap_or_default()
    };

    // Build share requests for remote custodians (not local, not already collected).
    let worker_kem_pub_b64_enc = crypto::b64_encode(&worker_kem_pub);
    let (_, coordinator_signing_priv) = match crypto::ensure_keypair_on_disk() {
        Ok(kp) => kp,
        Err(e) => {
            error!("dispatch_to_worker: failed to load signing keypair: {}", e);
            return;
        }
    };

    let mut remote_rxs: Vec<tokio::sync::mpsc::UnboundedReceiver<Option<crypto::ShareResponse>>> = vec![];

    for (custodian_idx, custodian_peer_id_str) in all_custodian_peers.iter().enumerate() {
        // Skip ourselves — already collected above.
        if custodian_peer_id_str == &local_peer_id {
            continue;
        }
        // Stop early if we already have enough shares.
        if worker_wrapped_shares.len() + remote_rxs.len() >= threshold {
            break;
        }

        let custodian_peer_id = match custodian_peer_id_str.parse::<libp2p::PeerId>() {
            Ok(id) => id,
            Err(e) => {
                warn!("dispatch_to_worker: invalid custodian peer_id '{}': {}", custodian_peer_id_str, e);
                continue;
            }
        };

        let nonce = crypto::b64_encode(&utils::make_nonce(Some("sr")).as_bytes().to_vec());
        let node_cert_bytes = crate::podmesh_p2p::control::deploy_dispatch::load_local_node_cert_bytes_for_peer(&local_peer_id);
        let tenant_owner_pubkey_b64 = protocol::NodeCert::from_bytes(&node_cert_bytes)
            .map(|c| c.owner_pubkey)
            .unwrap_or_default();
        let now_unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let share_index = (custodian_idx as u32) + 1;
        let authz_ctx = protocol::AuthzContext {
            tenant_owner_pubkey_b64,
            manifest_id: manifest_id.clone(),
            transport_peer_id: worker_peer_id_str.clone(),
            operation: protocol::AuthzOperation::ReleaseShare,
            http_path: None,
            dest_host: None,
            dest_port: None,
            worker_peer_id: Some(worker_peer_id_str.clone()),
            share_index: Some(share_index),
            delegate_peer_id: None,
            now_unix_secs,
        };
        let authz_token_b64 = match protocol::mint_release_share_token_b64(&coordinator_signing_priv, &authz_ctx) {
            Ok(t) => Some(t),
            Err(e) => {
                warn!("dispatch_to_worker: failed to mint release_share authz token: {}", e);
                continue;
            }
        };

        let mut req = crypto::ShareRequest {
            manifest_id: manifest_id.clone(),
            worker_peer_id: worker_peer_id_str.clone(),
            node_cert_bytes,
            assignment_sig: assignment_token.clone(),
            assigned_at_secs,
            share_index: Some(share_index),
            authz_token_b64,
            worker_kem_pub: worker_kem_pub_b64_enc.clone(),
            nonce,
            sig: String::new(),
        };
        let canonical = req.canonical_bytes();
        match crypto::sign_data_with_key(&coordinator_signing_priv, &canonical) {
            Ok(sig_bytes) => req.sig = crypto::b64_encode(&sig_bytes),
            Err(e) => {
                warn!("dispatch_to_worker: failed to sign ShareRequest: {}", e);
                continue;
            }
        }
        let req_bytes = req.to_bytes();
        let signed_req = match utils::sign_payload_default(&req_bytes, "share_request", Some("sreq")) {
            Ok(s) => s,
            Err(e) => {
                warn!("dispatch_to_worker: failed to wrap ShareRequest envelope: {}", e);
                continue;
            }
        };

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let out_id = swarm.behaviour_mut().scheduler_rr.send_request(&custodian_peer_id, signed_req);
        crate::podmesh_p2p::control::deploy_dispatch::insert_pending_share_request(
            &local_peer_id, &out_id, tx,
        );
        remote_rxs.push(rx);
        info!(
            "dispatch_to_worker: requested share for manifest_id={} from custodian={}",
            manifest_id, custodian_peer_id_str
        );
    }

    // Spawn a task to collect remote shares, then build and send the WorkloadDispatch.
    let sealed_spec_clone = sealed_spec.clone();
    let all_custodian_peers_clone = all_custodian_peers.clone();
    let required_caps = _required_capabilities.clone();
    tokio::spawn(async move {
        // Collect remote shares.
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(8_000);
        for mut rx in remote_rxs {
            if worker_wrapped_shares.len() >= threshold {
                break;
            }
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(Some(resp))) => {
                    worker_wrapped_shares.push(resp.wrapped_share);
                    info!(
                        "dispatch_to_worker: collected remote share {}/{} for manifest_id={}",
                        worker_wrapped_shares.len(), threshold, manifest_id
                    );
                }
                Ok(Some(None)) => {
                    warn!("dispatch_to_worker: custodian returned empty share for manifest_id={}", manifest_id);
                }
                Ok(None) | Err(_) => {
                    warn!("dispatch_to_worker: timed out waiting for remote share for manifest_id={}", manifest_id);
                }
            }
        }

        info!(
            "dispatch_to_worker: sending WorkloadDispatch for manifest_id={} to worker={} (wrapped_shares={})",
            manifest_id, worker_peer_id, worker_wrapped_shares.len()
        );

        let dispatch = WorkloadDispatch {
            sealed_spec: sealed_spec_clone.clone(),
            custodian_peers: all_custodian_peers_clone.clone(),
            coordinator_sig,
            worker_wrapped_shares,
            coordinator_peer_id: local_peer_id.clone(),
            assignment_token,
            assigned_at_secs,
        };

        let dispatch_bytes = dispatch.to_bytes();
        let signed_dispatch = match utils::sign_payload_default(&dispatch_bytes, "workload_dispatch", Some("wdisp")) {
            Ok(signed) => signed,
            Err(e) => {
                error!("dispatch_to_worker: failed to sign WorkloadDispatch: {}", e);
                return;
            }
        };

        // Re-enqueue as a SendSignedWorkloadDispatch so the swarm loop handles the actual send.
        crate::podmesh_p2p::control::enqueue_control(
            &local_peer_id,
            crate::podmesh_p2p::control::Libp2pControl::SendSignedWorkloadDispatch {
                worker_peer_id_str,
                worker_kem_pub_b64,
                sealed_spec: sealed_spec_clone,
                all_custodian_peers: all_custodian_peers_clone,
                required_capabilities: required_caps,
                signed_dispatch,
                dispatch,
            },
        );
    });
}

/// Send a pre-built signed WorkloadDispatch envelope to the worker.
/// Called after the coordinator has collected all remote shares asynchronously.
pub fn handle_send_signed_workload_dispatch(
    worker_peer_id_str: String,
    worker_kem_pub_b64: String,
    sealed_spec: SealedSpec,
    all_custodian_peers: Vec<String>,
    required_capabilities: Vec<String>,
    signed_dispatch: Vec<u8>,
    dispatch: WorkloadDispatch,
    swarm: &mut Swarm<MyBehaviour>,
) {
    let manifest_id = sealed_spec.manifest_id.clone();

    let worker_peer_id = match worker_peer_id_str.parse::<libp2p::PeerId>() {
        Ok(id) => id,
        Err(e) => {
            error!("dispatch_to_worker: invalid worker peer_id '{}': {}", worker_peer_id_str, e);
            return;
        }
    };

    let coordinator_peer_id = swarm.local_peer_id().to_string();

    // If the worker is the local node itself, dispatch directly without going over the network —
    // libp2p cannot dial itself and always produces a DialFailure.
    if worker_peer_id == *swarm.local_peer_id() {
        info!(
            "dispatch_to_worker: worker is local node, deploying WorkloadDispatch for manifest_id={} directly",
            manifest_id
        );
        crate::podmesh_p2p::control::enqueue_control(
            &coordinator_peer_id,
            crate::podmesh_p2p::control::Libp2pControl::DeployDispatchedWorkload {
                dispatch,
                worker_peer_id,
            },
        );
        return;
    }

    let out_id = swarm
        .behaviour_mut()
        .scheduler_rr
        .send_request(&worker_peer_id, signed_dispatch);

    let request_id_str = format!("{:?}", out_id);
    info!(
        "dispatch_to_worker: sent signed WorkloadDispatch for manifest_id={} to worker={} (request_id={})",
        manifest_id, worker_peer_id_str, request_id_str
    );

    register_workload_dispatch_send(
        coordinator_peer_id,
        request_id_str,
        worker_peer_id_str,
        worker_kem_pub_b64,
        sealed_spec,
        all_custodian_peers,
        required_capabilities,
    );
}

/// Register an outbound WorkloadDispatch request ID for retry-on-failure tracking.
pub fn register_workload_dispatch_send(
    coordinator_peer_id: String,
    request_id_str: String,
    worker_peer_id_str: String,
    worker_kem_pub_b64: String,
    sealed_spec: SealedSpec,
    all_custodian_peers: Vec<String>,
    required_capabilities: Vec<String>,
) {
    let key = format!("{}:{}", coordinator_peer_id, request_id_str);
    PENDING_DISPATCH_SENDS.lock().unwrap_or_else(|p| p.into_inner()).insert(
        key,
        (coordinator_peer_id, worker_peer_id_str, worker_kem_pub_b64, sealed_spec, all_custodian_peers, required_capabilities),
    );
}

/// Collect all custodian shares for a manifest (from the global store) and re-wrap
/// them for the worker's KEM pubkey.
fn collect_local_shares_for_worker(
    manifest_id: &str,
    worker_kem_pub: &[u8],
    local_peer_id: &str,
    _all_custodian_peers: &[String],
) -> Vec<Vec<u8>> {
    let store = match crate::storage::get_custodian_store() {
        Some(s) => s,
        None => return vec![],
    };

    let (_, kem_priv) = match crypto::ensure_kem_keypair_on_disk() {
        Ok(kp) => kp,
        Err(_) => return vec![],
    };

    // Use the peer-scoped record if available, otherwise fall back.
    let record = match store.get_record_for_peer(manifest_id, local_peer_id) {
        Ok(Some(r)) => r,
        _ => match store.get_record(manifest_id) {
            Ok(Some(r)) => r,
            _ => return vec![],
        },
    };

    // Decrypt the stored share with this node's KEM private key.
    let raw_share = match crypto::decrypt_payload_from_recipient_blob(&record.wrapped_share, &kem_priv) {
        Ok(s) => s,
        Err(e) => {
            warn!("dispatch_to_worker: failed to decrypt local share for {}: {}", manifest_id, e);
            return vec![];
        }
    };

    // Re-wrap for the worker.
    match crypto::encrypt_payload_for_recipient(worker_kem_pub, &raw_share) {
        Ok(wrapped) => vec![wrapped],
        Err(e) => {
            warn!("dispatch_to_worker: failed to re-wrap share for worker ({}): {}", manifest_id, e);
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workload_dispatch_roundtrip() {
        use protocol::machine::{SealedSpec, WorkloadDispatch};

        let spec = SealedSpec {
            manifest_id: "aabb".to_string(),
            owner_pubkey: "pub".to_string(),
            ciphertext: vec![1, 2, 3],
            nonce: vec![0u8; 24],
            kfrag_count: 3,
            kfrag_threshold: 2,
            sealed_at_secs: 0,
            submission_version: protocol::machine::SEAL_VERSION_V1,
            replica_count: 1,
        };

        let dispatch = WorkloadDispatch {
            sealed_spec: spec.clone(),
            custodian_peers: vec!["peer-1".to_string(), "peer-2".to_string()],
            coordinator_sig: "csig".to_string(),
            worker_wrapped_shares: vec![],
            coordinator_peer_id: String::new(),
            assignment_token: String::new(),
            assigned_at_secs: 0,
        };

        let bytes = dispatch.to_bytes();
        let decoded = WorkloadDispatch::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.sealed_spec.manifest_id, "aabb");
        assert_eq!(decoded.custodian_peers.len(), 2);
        assert_eq!(decoded.coordinator_sig, "csig");
    }
}
