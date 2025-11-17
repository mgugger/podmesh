use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, Swarm, autonat, gossipsub, identify, kad, relay, request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use p2p::{
    CoreBehaviourAccess, NodeConfig,
    handshake::{self, HandshakeDriveConfig, HandshakeState},
    request_response::ByteCodec,
};
use protocol::libp2p_constants::WORKLOAD_CLUSTER_TOPIC;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::config::Config;

type HandshakeCodec = ByteCodec;

const PROXY_PROVIDER_KEY: &str = "beemesh-proxy-node";

fn announce_proxy_provider(
    swarm: &mut Swarm<WorkloadBehaviour>,
    proxy_announced_tx: &watch::Sender<bool>,
) {
    let record_key = kad::RecordKey::new(&PROXY_PROVIDER_KEY);
    match swarm.behaviour_mut().kademlia.start_providing(record_key) {
        Ok(query_id) => {
            info!(
                peer = %swarm.local_peer_id(),
                ?query_id,
                "workload proxy provider announcement scheduled"
            );
            let _ = proxy_announced_tx.send(true);
        }
        Err(err) => {
            warn!(
                peer = %swarm.local_peer_id(),
                error = %err,
                "failed to announce workload proxy provider"
            );
        }
    }
}

#[derive(NetworkBehaviour)]
pub struct WorkloadBehaviour {
    pub gossipsub: gossipsub::Behaviour,
    pub handshake_rr: request_response::Behaviour<HandshakeCodec>,
    pub kademlia: kad::Behaviour<kad::store::MemoryStore>,
    pub relay: relay::Behaviour,
    pub autonat: autonat::Behaviour,
    pub identify: identify::Behaviour,
}

impl CoreBehaviourAccess for WorkloadBehaviour {
    fn gossipsub_mut(&mut self) -> &mut gossipsub::Behaviour {
        &mut self.gossipsub
    }

    fn kademlia_mut(&mut self) -> &mut kad::Behaviour<kad::store::MemoryStore> {
        &mut self.kademlia
    }
}

pub struct P2pNodeHandle {
    task: JoinHandle<()>,
    peer_rx: watch::Receiver<Vec<String>>,
    peer_id: String,
    kad_ready_rx: watch::Receiver<bool>,
    proxy_provider_announced_rx: watch::Receiver<bool>,
    _proxy_provider_announced_tx: watch::Sender<bool>,
}

impl P2pNodeHandle {
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    pub fn peer_rx(&self) -> watch::Receiver<Vec<String>> {
        self.peer_rx.clone()
    }

    pub fn kad_ready_rx(&self) -> watch::Receiver<bool> {
        self.kad_ready_rx.clone()
    }

    pub fn proxy_provider_announced_rx(&self) -> watch::Receiver<bool> {
        self.proxy_provider_announced_rx.clone()
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

pub fn spawn(cfg: &Config) -> Result<P2pNodeHandle> {
    let node_cfg = NodeConfig::new(
        cfg.libp2p_quic_port,
        cfg.libp2p_host.clone(),
        WORKLOAD_CLUSTER_TOPIC,
    );
    let (mut swarm, _topic, peer_rx, peer_tx) = p2p::setup_swarm(node_cfg, |key| {
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
            .expect("valid workload gossipsub config");
        let gossipsub = gossipsub::Behaviour::new(
            gossipsub::MessageAuthenticity::Signed(key.clone()),
            gossipsub_config,
        )
        .expect("create workload gossipsub behaviour");

        let handshake_rr = request_response::Behaviour::new(
            std::iter::once((
                handshake::HANDSHAKE_PROTOCOL,
                request_response::ProtocolSupport::Full,
            )),
            request_response::Config::default(),
        );

        let store = kad::store::MemoryStore::new(key.public().to_peer_id());
        let mut kademlia_config = kad::Config::default();
        kademlia_config.set_replication_factor(std::num::NonZeroUsize::new(3).unwrap());
        kademlia_config.set_max_packet_size(1024 * 1024);
        kademlia_config.set_parallelism(std::num::NonZeroUsize::new(3).unwrap());
        kademlia_config.set_query_timeout(Duration::from_secs(15));
        kademlia_config.set_provider_record_ttl(Some(Duration::from_secs(30)));
        kademlia_config.set_provider_publication_interval(Some(Duration::from_secs(5)));
        let mut kademlia =
            kad::Behaviour::with_config(key.public().to_peer_id(), store, kademlia_config);
        kademlia.set_mode(Some(kad::Mode::Server));

        let relay = relay::Behaviour::new(key.public().to_peer_id(), Default::default());
        let autonat = autonat::Behaviour::new(key.public().to_peer_id(), Default::default());
        let identify =
            identify::Behaviour::new(identify::Config::new("/beemesh/0.1.0".into(), key.public()));

        WorkloadBehaviour {
            gossipsub,
            handshake_rr,
            kademlia,
            relay,
            autonat,
            identify,
        }
    })?;

    let kad_protocols: Vec<String> = swarm
        .behaviour()
        .kademlia
        .protocol_names()
        .iter()
        .map(|p| p.to_string())
        .collect();
    warn!(
        peer = %swarm.local_peer_id(),
        ?kad_protocols,
        "workload kad protocols configured"
    );

    let (proxy_announced_tx, proxy_provider_announced_rx) = watch::channel(false);
    let runtime_proxy_announced_tx = proxy_announced_tx.clone();
    let enable_proxy_provider = cfg.enable_proxy_provider;
    if enable_proxy_provider {
        announce_proxy_provider(&mut swarm, &proxy_announced_tx);
    } else {
        warn!(peer = %swarm.local_peer_id(), "proxy provider announcement disabled");
    }

    let handshake_states: HashMap<PeerId, HandshakeState> = HashMap::new();
    let peer_protocols: HashMap<PeerId, String> = HashMap::new();
    let single_node_mode = cfg.bootstrap_peer_strings.is_empty();

    for addr in &cfg.bootstrap_peer_strings {
        match addr.parse::<Multiaddr>() {
            Ok(ma) => {
                register_initial_peer(&mut swarm, &ma, addr);
                if let Err(err) = swarm.dial(ma) {
                    warn!("failed to dial bootstrap {}: {}", addr, err);
                }
            }
            Err(err) => warn!("invalid bootstrap multiaddr {}: {}", addr, err),
        }
    }

    let (kad_ready_tx, kad_ready_rx) = watch::channel(false);
    let mut peer_tx = peer_tx;
    let local_peer_id = swarm.local_peer_id().to_string();
    let kad_ready_tx = kad_ready_tx;
    let task = tokio::spawn({
        let mut handshake_states = handshake_states;
        let mut peer_protocols = peer_protocols;
        let mut kad_bootstrapped = false;
        let mut kad_ready_signaled = false;
        let proxy_announced_tx = runtime_proxy_announced_tx;
        let enable_proxy_provider = enable_proxy_provider;
        let single_node_mode = single_node_mode;
        async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            let mut handshake_interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    event = swarm.select_next_some() => {
                        match event {
                            SwarmEvent::NewListenAddr { address, .. } => {
                                info!("workload libp2p listening on {}", address);
                                if single_node_mode && !kad_bootstrapped {
                                    kad_bootstrapped = true;
                                    kad_ready_signaled = true;
                                    warn!("single-node kademlia bootstrap ready");
                                    let _ = kad_ready_tx.send(true);
                                }
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::Gossipsub(gossipsub::Event::Subscribed { peer_id, .. })) => {
                                warn!("peer {} subscribed", peer_id);
                                publish_peer_snapshot(&swarm, &mut peer_tx);
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::Gossipsub(gossipsub::Event::Unsubscribed { peer_id, .. })) => {
                                warn!("peer {} unsubscribed", peer_id);
                                publish_peer_snapshot(&swarm, &mut peer_tx);
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::Kademlia(event)) => {
                                trace_kad(&event);
                                if !kad_ready_signaled {
                                    if let kad::Event::OutboundQueryProgressed { result, .. } = &event {
                                        if let kad::QueryResult::Bootstrap(Ok(ok)) = result {
                                            if ok.num_remaining == 0 {
                                                kad_ready_signaled = true;
                                                let _ = kad_ready_tx.send(true);
                                            }
                                        }
                                    }
                                }
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::HandshakeRr(request_response::Event::Message { message, peer, connection_id: _ })) => {
                                handshake::handle_request_response_message(
                                    message,
                                    peer,
                                    &mut handshake_states,
                                    |resp, channel| {
                                        let _ = swarm.behaviour_mut().handshake_rr.send_response(channel, resp);
                                    },
                                );
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::HandshakeRr(request_response::Event::OutboundFailure { peer, error, .. })) => {
                                warn!(%peer, ?error, "workload handshake outbound failure");
                                if matches!(error, request_response::OutboundFailure::UnsupportedProtocols) {
                                    handshake::track_peer(&mut handshake_states, &peer).confirmed = true;
                                    warn!(%peer, "handshake disabled for peer due to unsupported protocol");
                                }
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::HandshakeRr(request_response::Event::InboundFailure { peer, error, .. })) => {
                                warn!(%peer, ?error, "workload handshake inbound failure");
                            }
                            SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                                warn!("connection established with {}", peer_id);
                                handshake::track_peer(&mut handshake_states, &peer_id);
                                let addr = endpoint.get_remote_address().clone();
                                let addr_string = addr.to_string();
                                swarm
                                    .behaviour_mut()
                                    .kademlia
                                    .add_address(&peer_id, addr);
                                peer_protocols.insert(peer_id, addr_string);
                                log_peer_bootstrap_state(&peer_protocols, kad_bootstrapped);

                                let connected_count = swarm.connected_peers().count();
                                if connected_count >= 2 && !kad_bootstrapped {
                                    warn!(count = connected_count, "workload bootstrapping kademlia");
                                    if let Err(err) = swarm.behaviour_mut().kademlia.bootstrap() {
                                        error!(?err, "workload kademlia bootstrap failed");
                                    } else {
                                        kad_bootstrapped = true;
                                        warn!("workload kademlia bootstrap started");
                                        if enable_proxy_provider {
                                            announce_proxy_provider(&mut swarm, &proxy_announced_tx);
                                        }
                                        log_peer_bootstrap_state(&peer_protocols, kad_bootstrapped);
                                    }
                                }
                            }
                            SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
                                warn!("connection closed with {}", peer_id);
                                handshake::untrack_peer(&mut handshake_states, &peer_id);
                                if num_established == 0 {
                                    swarm.behaviour_mut().kademlia.remove_peer(&peer_id);
                                }
                                peer_protocols.remove(&peer_id);
                                log_peer_bootstrap_state(&peer_protocols, kad_bootstrapped);
                            }
                            _ => {}
                        }
                    }
                    _ = handshake_interval.tick() => {
                        let local_peer = swarm.local_peer_id().clone();
                        let mut pending_requests: Vec<(PeerId, Vec<u8>)> = Vec::new();
                        let mut dropped_peers: Vec<PeerId> = Vec::new();
                        if let Err(err) = handshake::drive_handshakes(
                            &mut handshake_states,
                            &local_peer,
                            &HandshakeDriveConfig::default(),
                            |peer, payload| {
                                pending_requests.push((peer.clone(), payload));
                                true
                            },
                            |peer| dropped_peers.push(peer.clone()),
                        ) {
                            warn!(?err, "workload handshake drive failed");
                        }

                        for (peer, payload) in pending_requests {
                            let request_id = swarm
                                .behaviour_mut()
                                .handshake_rr
                                .send_request(&peer, payload);
                            debug!(%peer, ?request_id, "workload handshake request sent");
                        }

                        for peer in dropped_peers {
                            swarm
                                .behaviour_mut()
                                .gossipsub
                                .remove_explicit_peer(&peer);
                        }
                    }
                    _ = interval.tick() => {
                        publish_peer_snapshot(&swarm, &mut peer_tx)
                    }
                }
            }
        }
    });

    Ok(P2pNodeHandle {
        task,
        peer_rx,
        peer_id: local_peer_id,
        kad_ready_rx,
        proxy_provider_announced_rx,
        _proxy_provider_announced_tx: proxy_announced_tx,
    })
}

fn publish_peer_snapshot(
    swarm: &Swarm<WorkloadBehaviour>,
    peer_tx: &mut watch::Sender<Vec<String>>,
) {
    let peers: Vec<String> = swarm
        .behaviour()
        .gossipsub
        .all_peers()
        .map(|(peer, _)| peer.to_string())
        .collect();
    let _ = peer_tx.send(peers);
}

fn trace_kad(event: &kad::Event) {
    match event {
        kad::Event::OutboundQueryProgressed { id, result, .. } => {
            debug!("kad query {:?} progressed: {:?}", id, result);
        }
        kad::Event::RoutingUpdated { peer, .. } => {
            debug!("kad routing updated for {}", peer);
        }
        kad::Event::InboundRequest { request } => match request {
            kad::InboundRequest::GetProvider {
                num_closer_peers,
                num_provider_peers,
            } => {
                warn!(
                    num_closer_peers,
                    num_provider_peers, "kad inbound get_provider request"
                );
            }
            _ => {}
        },
        _ => {}
    }
}

fn register_initial_peer(swarm: &mut Swarm<WorkloadBehaviour>, addr: &Multiaddr, raw: &str) {
    if let Some((peer_id, base_addr)) = extract_peer_id(addr.clone()) {
        {
            let behaviour = swarm.behaviour_mut();
            behaviour.kademlia.add_address(&peer_id, base_addr.clone());
            behaviour.gossipsub.add_explicit_peer(&peer_id);
        }
        warn!(
            %peer_id,
            bootstrap_protocol = %base_addr,
            address = %raw,
            "registered bootstrap peer as initial peer"
        );
    }
}

fn log_peer_bootstrap_state(peer_protocols: &HashMap<PeerId, String>, kad_started: bool) {
    if peer_protocols.is_empty() {
        warn!(kad_started, "no connected peers to report");
        return;
    }

    for (peer, protocol) in peer_protocols {
        warn!(
            peer = %peer,
            bootstrap_protocol = %protocol,
            kad_started,
            "workload peer bootstrap state"
        );
    }
}

fn extract_peer_id(mut addr: Multiaddr) -> Option<(PeerId, Multiaddr)> {
    match addr.pop() {
        Some(libp2p::multiaddr::Protocol::P2p(peer_id)) => Some((peer_id, addr)),
        _ => None,
    }
}
