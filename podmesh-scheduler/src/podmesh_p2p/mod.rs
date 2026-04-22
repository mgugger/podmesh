use anyhow::Result;

use futures::stream::StreamExt;
use libp2p::{
    PeerId, Swarm, gossipsub, kad, multiaddr::Multiaddr, multiaddr::Protocol, request_response,
    swarm::SwarmEvent,
};
use libp2p::{autonat, identify, relay};
use log::{debug, info, warn};
use once_cell::sync::OnceCell;
use std::collections::HashMap as StdHashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::{self, Interval};

use p2p::{
    NodeConfig,
    handshake::{self, HandshakeDriveConfig, HandshakeState},
};
use protocol::libp2p_constants::MACHINE_CLUSTER_TOPIC;

use crate::NodeMode;

// Global control sender for distributed operations
static CONTROL_SENDER: OnceCell<mpsc::UnboundedSender<control::Libp2pControl>> = OnceCell::new();
static DISABLED_SCHEDULING: OnceCell<Mutex<StdHashMap<PeerId, bool>>> = OnceCell::new();
static NODE_MODE: std::sync::OnceLock<NodeMode> = std::sync::OnceLock::new();
/// Maps peer_id -> a known multiaddr, populated on every connection established.
static PEER_ADDRESSES: OnceCell<Mutex<StdHashMap<PeerId, Multiaddr>>> = OnceCell::new();

/// Returns the first known multiaddr for a peer, if any.
pub fn get_peer_address(peer_id: &PeerId) -> Option<Multiaddr> {
    PEER_ADDRESSES
        .get()
        .and_then(|m| m.lock().ok())
        .and_then(|map| map.get(peer_id).cloned())
}

/// Set the operating mode for this node. Must be called before `setup_libp2p_node`.
pub fn set_node_mode(mode: NodeMode) {
    let _ = NODE_MODE.set(mode);
}

/// Get the operating mode for this node (defaults to `Both`).
pub fn get_node_mode() -> NodeMode {
    NODE_MODE.get().copied().unwrap_or(NodeMode::Both)
}

fn scheduling_map() -> &'static Mutex<StdHashMap<PeerId, bool>> {
    DISABLED_SCHEDULING.get_or_init(|| Mutex::new(StdHashMap::new()))
}

fn extract_listen_endpoint(addr: &Multiaddr) -> Option<(String, u16)> {
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    for proto in addr.iter() {
        match proto {
            Protocol::Ip4(ipv4) => host = Some(ipv4.to_string()),
            Protocol::Ip6(ipv6) => host = Some(ipv6.to_string()),
            Protocol::Dns(dns) => host = Some(dns.to_string()),
            Protocol::Dns4(dns) => host = Some(dns.to_string()),
            Protocol::Dns6(dns) => host = Some(dns.to_string()),
            Protocol::Tcp(value) | Protocol::Udp(value) => port = Some(value),
            _ => {}
        }
    }
    host.zip(port)
}

pub fn register_scheduling_preference(peer: PeerId, disabled: bool) {
    let mut map = scheduling_map()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.insert(peer, disabled);
}

pub fn is_scheduling_disabled_for(peer: &PeerId) -> bool {
    scheduling_map()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(peer)
        .copied()
        .unwrap_or(false)
}

use crate::podmesh_p2p::{
    behaviour::{MyBehaviour, MyBehaviourEvent},
    control::Libp2pControl,
};

pub mod behaviour;
pub mod capacity;
pub mod control;
pub mod envelope;
pub mod reply;
pub mod security;
pub mod utils;

pub fn setup_libp2p_node(
    quic_port: u16,
    host: &str,
    disable_scheduling: bool,
) -> Result<(
    Swarm<MyBehaviour>,
    gossipsub::IdentTopic,
    watch::Receiver<Vec<String>>,
    watch::Sender<Vec<String>>,
)> {
    let node_config = NodeConfig::new(quic_port, host.to_string(), MACHINE_CLUSTER_TOPIC);
    let (swarm, topic, peer_rx, peer_tx) = p2p::setup_swarm(node_config, |key| {
        debug!("Local PeerId: {}", key.public().to_peer_id());
        let gossipsub = p2p::create_gossipsub_behaviour(key)
            .expect("create gossipsub behaviour");

        let apply_rr = request_response::Behaviour::new(
            std::iter::once((
                "/podmesh/apply/1.0.0",
                request_response::ProtocolSupport::Full,
            )),
            request_response::Config::default()
                .with_request_timeout(std::time::Duration::from_secs(60)),
        );

        let handshake_rr = request_response::Behaviour::new(
            std::iter::once((
                "/podmesh/handshake/1.0.0",
                request_response::ProtocolSupport::Full,
            )),
            request_response::Config::default()
                .with_request_timeout(std::time::Duration::from_secs(5)),
        );

        let scheduler_rr = request_response::Behaviour::new(
            std::iter::once((
                "/podmesh/scheduler-tasks/1.0.0",
                request_response::ProtocolSupport::Full,
            )),
            request_response::Config::default()
                .with_request_timeout(std::time::Duration::from_secs(5)),
        );

        let delete_rr = request_response::Behaviour::new(
            std::iter::once((
                "/podmesh/delete/1.0.0",
                request_response::ProtocolSupport::Full,
            )),
            request_response::Config::default()
                .with_request_timeout(std::time::Duration::from_secs(5)),
        );

        let store = kad::store::MemoryStore::new(key.public().to_peer_id());
        let kademlia_config = p2p::default_kademlia_config();

        let mut kademlia =
            kad::Behaviour::with_config(key.public().to_peer_id(), store, kademlia_config);
        
        // Set Kademlia to server mode so nodes can store and serve provider records from other peers
        kademlia.set_mode(Some(kad::Mode::Server));

        let relay = relay::Behaviour::new(key.public().to_peer_id(), Default::default());
        let autonat = autonat::Behaviour::new(key.public().to_peer_id(), Default::default());
        let identify =
            identify::Behaviour::new(identify::Config::new("/podmesh/0.1.0".into(), key.public()));

        MyBehaviour {
            gossipsub,
            apply_rr,
            handshake_rr,
            scheduler_rr,
            delete_rr,
            kademlia,
            relay,
            autonat,
            identify,
        }
    })?;

    register_scheduling_preference(swarm.local_peer_id().clone(), disable_scheduling);

    Ok((swarm, topic, peer_rx, peer_tx))
}

// Global node keypair set at startup by machine::main
pub static NODE_KEYPAIR: OnceCell<Option<(Vec<u8>, Vec<u8>)>> = OnceCell::new();

pub fn set_node_keypair(pair: Option<(Vec<u8>, Vec<u8>)>) {
    let _ = NODE_KEYPAIR.set(pair);
}

/// Set the global control sender for distributed operations
pub fn set_control_sender(sender: mpsc::UnboundedSender<control::Libp2pControl>) {
    let _ = CONTROL_SENDER.set(sender);
}

/// Get the global control sender for distributed operations
pub fn get_control_sender() -> Option<&'static mpsc::UnboundedSender<control::Libp2pControl>> {
    CONTROL_SENDER.get()
}

/// Set whether scheduler request handling is disabled for this node
pub async fn start_libp2p_node(
    swarm: Swarm<MyBehaviour>,
    topic: gossipsub::IdentTopic,
    peer_tx: watch::Sender<Vec<String>>,
    control_rx: mpsc::UnboundedReceiver<Libp2pControl>,
) -> Result<()> {
    SwarmDriver::new(swarm, topic, peer_tx, control_rx)
        .run()
        .await
}

type HandshakeMap = std::collections::HashMap<PeerId, HandshakeState>;

struct SwarmDriver {
    swarm: Swarm<MyBehaviour>,
    topic: gossipsub::IdentTopic,
    peer_tx: watch::Sender<Vec<String>>,
    control_rx: mpsc::UnboundedReceiver<Libp2pControl>,
    pending_queries: StdHashMap<String, Vec<mpsc::UnboundedSender<String>>>,
    handshake_states: HandshakeMap,
    handshake_interval: Interval,
    renew_interval: Interval,
    heartbeat_interval: Interval,
    handshake_config: HandshakeDriveConfig,
}

impl SwarmDriver {
    fn new(
        swarm: Swarm<MyBehaviour>,
        topic: gossipsub::IdentTopic,
        peer_tx: watch::Sender<Vec<String>>,
        control_rx: mpsc::UnboundedReceiver<Libp2pControl>,
    ) -> Self {
        Self {
            swarm,
            topic,
            peer_tx,
            control_rx,
            pending_queries: StdHashMap::new(),
            handshake_states: HandshakeMap::new(),
            handshake_interval: time::interval(Duration::from_secs(1)),
            renew_interval: time::interval(Duration::from_millis(500)),
            heartbeat_interval: time::interval(Duration::from_secs(
                crate::custodian::heartbeat::HEARTBEAT_INTERVAL_SECS,
            )),
            handshake_config: HandshakeDriveConfig::default(),
        }
    }

    async fn run(mut self) -> Result<()> {
        loop {
            tokio::select! {
                maybe_msg = self.control_rx.recv() => {
                    if !self.process_control_message(maybe_msg).await {
                        break;
                    }
                }
                event = self.swarm.select_next_some() => {
                    self.handle_swarm_event(event).await;
                }
                _ = self.handshake_interval.tick() => self.drive_handshakes().await,
                _ = self.renew_interval.tick() => self.handle_renewals().await,
                _ = self.heartbeat_interval.tick() => self.handle_heartbeat_tick().await,
            }
        }

        Ok(())
    }

    async fn process_control_message(&mut self, maybe_msg: Option<Libp2pControl>) -> bool {
        if let Some(msg) = maybe_msg {
            control::handle_control_message(
                msg,
                &mut self.swarm,
                &self.topic,
                &mut self.pending_queries,
            )
            .await;
            true
        } else {
            info!("control channel closed; withdrawing provider announcements");
            control::withdraw_all_providers(&mut self.swarm);
            false
        }
    }

    async fn handle_swarm_event(&mut self, event: SwarmEvent<MyBehaviourEvent>) {
        match event {
            SwarmEvent::Behaviour(event) => self.handle_behaviour_event(event).await,
            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                // Only record the address if we are the dialer — the remote address is then
                // the peer's listen address, which other nodes can use to dial them.
                // When we are the listener, `get_remote_address()` is the dialer's ephemeral port.
                let addr = endpoint.get_remote_address().clone();
                self.on_connection_established(peer_id, addr, endpoint.is_dialer());
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                num_established,
                ..
            } => self.on_connection_closed(peer_id, num_established),
            SwarmEvent::NewListenAddr { address, .. } => self.on_new_listen_addr(address),
            _ => {}
        }
    }

    async fn handle_behaviour_event(&mut self, event: MyBehaviourEvent) {
        match event {
            MyBehaviourEvent::HandshakeRr(event) => self.handle_handshake_rr_event(event),
            MyBehaviourEvent::ApplyRr(event) => self.handle_apply_rr_event(event).await,
            MyBehaviourEvent::DeleteRr(event) => self.handle_delete_rr_event(event).await,
            MyBehaviourEvent::Gossipsub(event) => self.handle_gossipsub_event(event),
            MyBehaviourEvent::Kademlia(event) => behaviour::kademlia_event(event, None, &self.swarm.local_peer_id().to_string()),
            MyBehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. }) => {
                // Use the listen addresses from Identify to update PEER_ADDRESSES.
                // Prefer loopback (127.0.0.1) addresses so in-process tests work reliably;
                // fall back to the first address in the list.
                let addrs = info.listen_addrs;
                let best = addrs.iter()
                    .find(|a| a.to_string().contains("/ip4/127.0.0.1/"))
                    .or_else(|| addrs.first())
                    .cloned();
                if let Some(addr) = best {
                    PEER_ADDRESSES
                        .get_or_init(|| Mutex::new(StdHashMap::new()))
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(peer_id, addr.clone());
                    self.swarm.add_peer_address(peer_id, addr.clone());
                    self.swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
                }
            }
            MyBehaviourEvent::Identify(_) => {}
            MyBehaviourEvent::SchedulerRr(event) => {
                self.handle_scheduler_rr_event(event).await;
            }
            _ => {}
        }
    }

    fn handle_handshake_rr_event(&mut self, event: request_response::Event<Vec<u8>, Vec<u8>>) {
        match event {
            request_response::Event::Message { message, peer, .. } => {
                // Check if this is a response to a KEM pubkey fetch request
                if let request_response::Message::Response { response, .. } = &message {
                    if let Some(sender) = control::take_pending_kem_pubkey_request(&peer) {
                        let kem_pubkey = p2p::handshake::extract_kem_pubkey_from_response(response, &peer);
                        log::info!(
                            "Extracted KEM pubkey from handshake response from peer {}: {}",
                            peer,
                            kem_pubkey.is_some()
                        );
                        let _ = sender.send(kem_pubkey);
                    }
                }
                
                handshake::handle_request_response_message(
                    message,
                    peer,
                    &mut self.handshake_states,
                    |resp, channel| {
                        let _ = self
                            .swarm
                            .behaviour_mut()
                            .handshake_rr
                            .send_response(channel, resp);
                    },
                );
            }
            request_response::Event::OutboundFailure { peer, error, .. } => {
                // If there was a pending KEM pubkey request, notify of failure
                if let Some(sender) = control::take_pending_kem_pubkey_request(&peer) {
                    log::warn!("KEM pubkey fetch failed for peer {}: {:?}", peer, error);
                    let _ = sender.send(None);
                }
                behaviour::handshake_outbound_failure(peer, error);
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                behaviour::handshake_inbound_failure(peer, error);
            }
            request_response::Event::ResponseSent { .. } => {}
        }
    }

    async fn handle_apply_rr_event(&mut self, event: request_response::Event<Vec<u8>, Vec<u8>>) {
        match event {
            request_response::Event::Message { message, peer, .. } => {
                let local_peer = *self.swarm.local_peer_id();
                crate::workload_integration::handle_apply_message_with_workload_manager(
                    message,
                    peer,
                    &mut self.swarm,
                    local_peer,
                )
                .await;
            }
            request_response::Event::OutboundFailure { peer, error, .. } => {
                behaviour::apply_outbound_failure(peer, error);
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                behaviour::apply_inbound_failure(peer, error);
            }
            request_response::Event::ResponseSent { .. } => {}
        }
    }

    async fn handle_delete_rr_event(&mut self, event: request_response::Event<Vec<u8>, Vec<u8>>) {
        match event {
            request_response::Event::Message { message, peer, .. } => {
                let local_peer = *self.swarm.local_peer_id();
                behaviour::delete_message(message, peer, &mut self.swarm, local_peer).await;
            }
            request_response::Event::OutboundFailure { peer, error, .. } => {
                behaviour::delete_outbound_failure(peer, error);
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                behaviour::delete_inbound_failure(peer, error);
            }
            request_response::Event::ResponseSent { .. } => {}
        }
    }

    async fn handle_scheduler_rr_event(
        &mut self,
        event: request_response::Event<Vec<u8>, Vec<u8>>,
    ) {
        match event {
            request_response::Event::Message { message, peer, .. } => {
                let local_peer = *self.swarm.local_peer_id();
                behaviour::scheduler_message(
                    message,
                    peer,
                    &mut self.swarm,
                    local_peer,
                    &mut self.pending_queries,
                );
            }
            request_response::Event::OutboundFailure { peer, error, request_id, .. } => {
                warn!(
                    "libp2p: scheduler request outbound failure for peer {}: {:?}",
                    peer, error
                );
                let request_id_str = format!("{:?}", request_id);
                let local_peer_id = self.swarm.local_peer_id().to_string();
                crate::podmesh_p2p::control::dispatch_to_worker::handle_workload_dispatch_outbound_failure(&local_peer_id, &request_id_str);
                // If this was a ShareRequest, signal the waiting task so it can fail fast.
                crate::podmesh_p2p::control::deploy_dispatch::notify_share_request_failed(&local_peer_id, &request_id);
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                warn!(
                    "libp2p: scheduler request inbound failure for peer {}: {:?}",
                    peer, error
                );
            }
            request_response::Event::ResponseSent { .. } => {}
        }
    }

    fn handle_gossipsub_event(&mut self, event: gossipsub::Event) {
        match event {
            gossipsub::Event::Message {
                propagation_source,
                message,
                ..
            } => {
                behaviour::gossipsub_message(
                    propagation_source,
                    message,
                    self.topic.hash().clone(),
                    &mut self.swarm,
                    &mut self.pending_queries,
                );
            }
            gossipsub::Event::Subscribed { peer_id, topic } => {
                behaviour::gossipsub_subscribed(peer_id, topic);
            }
            gossipsub::Event::Unsubscribed { peer_id, topic } => {
                behaviour::gossipsub_unsubscribed(peer_id, topic);
            }
            _ => {}
        }
    }

    fn on_connection_established(&mut self, peer_id: PeerId, address: Multiaddr, is_dialer: bool) {
        info!(
            "DHT: Connection established with peer {}, adding to Kademlia",
            peer_id
        );
        handshake::track_peer(&mut self.handshake_states, &peer_id);
        self.swarm
            .behaviour_mut()
            .kademlia
            .add_address(&peer_id, address.clone());

        // Only store the peer's listen address when we are the dialer — when we are the
        // listener, `address` is the dialer's ephemeral source port and is not dialable.
        if is_dialer {
            PEER_ADDRESSES
                .get_or_init(|| Mutex::new(StdHashMap::new()))
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(peer_id, address);
        }

        let connected_peers: Vec<_> = self.swarm.connected_peers().cloned().collect();
        if connected_peers.len() >= 2 {
            info!(
                "DHT: Bootstrapping with {} connected peers",
                connected_peers.len()
            );
            let _ = self.swarm.behaviour_mut().kademlia.bootstrap();
        }
    }

    fn on_connection_closed(&mut self, peer_id: PeerId, num_established: u32) {
        if num_established == 0 {
            info!(
                "DHT: All connections to peer {} closed, removing from Kademlia",
                peer_id
            );
            self.swarm.behaviour_mut().kademlia.remove_peer(&peer_id);
            handshake::untrack_peer(&mut self.handshake_states, &peer_id);
        }
    }

    fn on_new_listen_addr(&mut self, address: Multiaddr) {
        if let Some((host, port)) = extract_listen_endpoint(&address) {
            info!("libp2p: listening on {}:{}", host, port);
        } else {
            info!("libp2p: listening on {address}");
        }
    }

    async fn drive_handshakes(&mut self) {
        let local_peer = self.swarm.local_peer_id().clone();
        match handshake::collect_handshake_actions(
            &mut self.handshake_states,
            &local_peer,
            &self.handshake_config,
        ) {
            Ok(actions) => {
                for (peer, payload) in actions.requests {
                    let request_id = self
                        .swarm
                        .behaviour_mut()
                        .handshake_rr
                        .send_request(&peer, payload);
                    debug!(
                        "libp2p: sent handshake request to peer={} request_id={:?}",
                        peer, request_id
                    );
                }

                for peer in actions.drops {
                    self.swarm
                        .behaviour_mut()
                        .gossipsub
                        .remove_explicit_peer(&peer);
                }
            }
            Err(err) => warn!("handshake drive failed: {err:?}"),
        }

        let all_peers: Vec<String> = self
            .swarm
            .behaviour()
            .gossipsub
            .all_peers()
            .map(|(p, _topics)| p.to_string())
            .collect();
        let _ = self.peer_tx.send(all_peers);
    }

    async fn handle_renewals(&mut self) {
        control::drain_enqueued_controls(&mut self.swarm, &self.topic, &mut self.pending_queries)
            .await;
        control::renew_due_providers(&mut self.swarm);
    }

    /// Phase 6: broadcast heartbeat if custodian; check for and evict expired peers.
    async fn handle_heartbeat_tick(&mut self) {
        use crate::custodian::heartbeat;

        if get_node_mode().is_custodian() {
            // Collect manifest IDs we hold shares for
            let manifest_ids = crate::storage::get_custodian_store()
                .map(|store| store.manifest_ids().unwrap_or_default())
                .unwrap_or_default();

            let local_peer_id = self.swarm.local_peer_id().to_string();
            match heartbeat::build_heartbeat_ping(&local_peer_id, manifest_ids) {
                Ok(ping) => {
                    let ping_bytes = ping.to_bytes();
                    match behaviour::gossipsub_message::sign_and_publish(
                        &mut self.swarm,
                        &self.topic,
                        &ping_bytes,
                        "heartbeat_ping",
                    ) {
                        Ok(_) => debug!("heartbeat: broadcast for peer={}", local_peer_id),
                        Err(e) => warn!("heartbeat: publish failed: {:?}", e),
                    }
                }
                Err(e) => warn!("heartbeat: build_heartbeat_ping failed: {}", e),
            }
        }

        // Expiry check: emit CustodianWithdraw for dead custodians
        let expired = heartbeat::expired_custodians();
        for (dead_peer_id, manifest_ids) in expired {
            warn!(
                "heartbeat: custodian {} has expired (no heartbeat for {}s)",
                dead_peer_id, heartbeat::HEARTBEAT_EXPIRY_SECS
            );
            heartbeat::remove_liveness_entry(&dead_peer_id);

            // Broadcast CustodianWithdraw for each manifest this peer held
            for manifest_id in &manifest_ids {
                self.broadcast_custodian_withdraw(&dead_peer_id, manifest_id).await;
            }
        }
    }

    /// Broadcast a `CustodianWithdraw` for a given peer+manifest on the custodian topic.
    async fn broadcast_custodian_withdraw(&mut self, peer_id: &str, manifest_id: &str) {
        use protocol::machine::CustodianWithdraw;
        use std::time::{SystemTime, UNIX_EPOCH};

        let timestamp_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut withdraw = CustodianWithdraw {
            manifest_id: manifest_id.to_string(),
            owner_pubkey: String::new(), // best-effort — we don't have owner pubkey here
            custodian_peer_id: peer_id.to_string(),
            timestamp_secs,
            sig: String::new(),
        };

        // Sign if we have a keypair
        if let Ok((_, sk)) = crypto::ensure_keypair_on_disk() {
            let canonical = withdraw.canonical_bytes();
            if let Ok(sig_bytes) = crypto::sign_data_with_key(&sk, &canonical) {
                withdraw.sig = crypto::b64_encode(&sig_bytes);
            }
        }

        let withdraw_bytes = withdraw.to_bytes();
        if let Err(e) = behaviour::gossipsub_message::sign_and_publish(
            &mut self.swarm,
            &self.topic,
            &withdraw_bytes,
            "custodian_withdraw",
        ) {
            warn!("heartbeat: failed to publish CustodianWithdraw for peer={} manifest={}: {:?}", peer_id, manifest_id, e);
        } else {
            info!(
                "heartbeat: broadcast CustodianWithdraw for dead peer={} manifest={}",
                peer_id, manifest_id
            );
        }
    }
}
