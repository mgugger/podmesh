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
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    time::Duration,
};
use tokio::sync::{mpsc, watch};
use tokio::time::{self, Interval};

use p2p::{
    NodeConfig,
    handshake::{self, HandshakeDriveConfig, HandshakeState},
};
use protocol::libp2p_constants::MACHINE_CLUSTER_TOPIC;

// Global control sender for distributed operations
static CONTROL_SENDER: OnceCell<mpsc::UnboundedSender<control::Libp2pControl>> = OnceCell::new();
static DISABLED_SCHEDULING: OnceCell<Mutex<StdHashMap<PeerId, bool>>> = OnceCell::new();

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
    let mut map = scheduling_map().lock().unwrap();
    map.insert(peer, disabled);
}

pub fn is_scheduling_disabled_for(peer: &PeerId) -> bool {
    scheduling_map()
        .lock()
        .unwrap()
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
        let message_id_fn = |message: &gossipsub::Message| {
            let mut s = DefaultHasher::new();
            message.data.hash(&mut s);
            gossipsub::MessageId::from(s.finish().to_string())
        };
        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(10))
            .validation_mode(gossipsub::ValidationMode::Strict)
            .mesh_n_low(1)
            .mesh_n(3)
            .mesh_n_high(6)
            .mesh_outbound_min(1)
            .message_id_fn(message_id_fn)
            .allow_self_origin(true)
            .build()
            .expect("valid gossipsub config");
        let gossipsub = gossipsub::Behaviour::new(
            gossipsub::MessageAuthenticity::Signed(key.clone()),
            gossipsub_config,
        )
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
            request_response::Config::default(),
        );

        let scheduler_rr = request_response::Behaviour::new(
            std::iter::once((
                "/podmesh/scheduler-tasks/1.0.0",
                request_response::ProtocolSupport::Full,
            )),
            request_response::Config::default(),
        );

        let delete_rr = request_response::Behaviour::new(
            std::iter::once((
                "/podmesh/delete/1.0.0",
                request_response::ProtocolSupport::Full,
            )),
            request_response::Config::default(),
        );

        let store = kad::store::MemoryStore::new(key.public().to_peer_id());
        let mut kademlia_config = kad::Config::default();
        // Use replication factor of 3 to ensure provider records reach all nodes in small networks
        kademlia_config.set_replication_factor(std::num::NonZeroUsize::new(3).unwrap());
        kademlia_config.set_max_packet_size(1024 * 1024);
        kademlia_config.set_parallelism(std::num::NonZeroUsize::new(3).unwrap());
        kademlia_config.set_query_timeout(std::time::Duration::from_secs(15));
        kademlia_config.set_provider_record_ttl(Some(std::time::Duration::from_secs(30)));
        kademlia_config.set_provider_publication_interval(Some(std::time::Duration::from_secs(5)));

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
                self.on_connection_established(peer_id, endpoint.get_remote_address().clone());
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
            MyBehaviourEvent::DeleteRr(event) => self.handle_delete_rr_event(event),
            MyBehaviourEvent::Gossipsub(event) => self.handle_gossipsub_event(event),
            MyBehaviourEvent::Kademlia(event) => behaviour::kademlia_event(event, None),
            MyBehaviourEvent::SchedulerRr(event) => {
                self.handle_scheduler_rr_event(event).await;
            }
            _ => {}
        }
    }

    fn handle_handshake_rr_event(&mut self, event: request_response::Event<Vec<u8>, Vec<u8>>) {
        match event {
            request_response::Event::Message { message, peer, .. } => {
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

    fn handle_delete_rr_event(&mut self, event: request_response::Event<Vec<u8>, Vec<u8>>) {
        match event {
            request_response::Event::Message { message, peer, .. } => {
                let local_peer = *self.swarm.local_peer_id();
                behaviour::delete_message(message, peer, &mut self.swarm, local_peer);
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
            request_response::Event::OutboundFailure { peer, error, .. } => {
                warn!(
                    "libp2p: scheduler request outbound failure for peer {}: {:?}",
                    peer, error
                );
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

    fn on_connection_established(&mut self, peer_id: PeerId, address: Multiaddr) {
        info!(
            "DHT: Connection established with peer {}, adding to Kademlia",
            peer_id
        );
        handshake::track_peer(&mut self.handshake_states, &peer_id);
        self.swarm
            .behaviour_mut()
            .kademlia
            .add_address(&peer_id, address);

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
}
