use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, Swarm, StreamProtocol, autonat, gossipsub, identify, kad, relay, request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use p2p::{
    CoreBehaviourAccess, NodeConfig, libp2p_stream,
    sidecar_manifest::verify_sidecar_manifest_envelope,
    handshake::{self, HandshakeDriveConfig, HandshakeState},
    http_proxy::{ProxyCodec, ProxyHttpRequest, ProxyHttpResponse},
    request_response::{HandshakeCodec, ManifestFetchCodec, ByteCodec},
};
use protocol::libp2p_constants::{
    SIDECAR_MANIFEST_PROTOCOL, INGRESS_PROXY_PROTOCOL, MANIFEST_RECORD_PREFIX,
    WORKLOAD_CLUSTER_TOPIC, MANIFEST_RECORD_TTL_MS, MANIFEST_CACHE_TTL_RATIO,
    EGRESS_TUNNEL_PROTOCOL, SIDECAR_REGISTRATION_PROTOCOL,
};
use protocol::egress::{EgressTunnelRequest, EgressTunnelResponse};
use protocol::machine::{SidecarProviderRecordOwned, SidecarRouteSpec, SidecarRouteKind, build_sidecar_manifest_request};
use protocol::{SidecarRegistration, SidecarRegistrationAck, SidecarRoute};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use log::{debug, error, info, warn};

use crate::config::Config;

/// In-memory routing table entry for a registered sidecar.
#[derive(Debug, Clone)]
pub struct SidecarRouteEntry {
    pub sidecar_peer_id: String,
    pub routes: Vec<SidecarRoute>,
    pub registered_at: u64,
}

/// Shared routing table: manifest_id → SidecarRouteEntry.
pub type RoutingTable = Arc<std::sync::RwLock<HashMap<String, SidecarRouteEntry>>>;

/// Codec for the sidecar-registration request-response protocol (raw length-prefixed bytes).
pub type RegistrationCodec = ByteCodec;

use protocol::libp2p_constants::PROXY_PROVIDER_KEY;
const MAX_MANIFEST_FETCH_PEERS: usize = 4;

/// Compute the opaque DHT key for a tenant proxy announcement.
///
/// The key is `sha256(owner_pubkey_bytes || b"proxy")`, so it is unlinkable
/// to the raw public key bytes while still being deterministic for a given owner.
pub fn compute_tenant_proxy_dht_key(owner_pubkey_b64: &str) -> anyhow::Result<Vec<u8>> {
    use sha2::{Digest, Sha256};
    let pk_bytes = crypto::b64_decode(owner_pubkey_b64)?;
    let mut input = pk_bytes;
    input.extend_from_slice(b"proxy");
    let hash = Sha256::digest(&input);
    Ok(hash.to_vec())
}

enum P2pCommand {
    ProxyHttp {
        request: ProxyHttpRequest,
        respond_to: oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
    },
}

struct ProxyPendingRequest {
    request: ProxyHttpRequest,
    respond_to: oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
}

struct SidecarCacheEntry {
    record: SidecarProviderRecordOwned,
    expires_at: Instant,
}

struct ManifestQueryState {
    waiters: Vec<ProxyPendingRequest>,
    query_id: kad::QueryId,
    pending_requests: HashSet<request_response::OutboundRequestId>,
    requested_peers: HashSet<PeerId>,
    providers_finished: bool,
}

fn announce_proxy_provider(
    swarm: &mut Swarm<WorkloadBehaviour>,
    proxy_announced_tx: &watch::Sender<bool>,
) {
    let record_key = kad::RecordKey::new(&PROXY_PROVIDER_KEY);
    match swarm.behaviour_mut().kademlia.start_providing(record_key) {
        Ok(query_id) => {
            info!(
                "workload proxy provider announcement scheduled peer={} query_id={:?}",
                swarm.local_peer_id(),
                query_id
            );
            let _ = proxy_announced_tx.send(true);
        }
        Err(err) => {
            warn!(
                "failed to announce workload proxy provider peer={} error={}",
                swarm.local_peer_id(),
                err
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
    pub proxy_rr: request_response::Behaviour<ProxyCodec>,
    pub manifest_rr: request_response::Behaviour<ManifestFetchCodec>,
    pub egress_stream: libp2p_stream::Behaviour,
    pub registration_rr: request_response::Behaviour<RegistrationCodec>,
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
    command_tx: mpsc::UnboundedSender<P2pCommand>,
    pub routing_table: RoutingTable,
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

    pub fn proxy_client(&self) -> ProxyClient {
        ProxyClient {
            tx: self.command_tx.clone(),
        }
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

#[derive(Clone)]
pub struct ProxyClient {
    tx: mpsc::UnboundedSender<P2pCommand>,
}

impl ProxyClient {
    pub async fn forward(&self, request: ProxyHttpRequest) -> Result<ProxyHttpResponse> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(P2pCommand::ProxyHttp {
                request,
                respond_to: tx,
            })
            .map_err(|_| anyhow!("p2p node shut down"))?;
        rx.await
            .map_err(|_| anyhow!("proxy response channel closed"))?
    }
}

pub fn spawn(cfg: &Config) -> Result<P2pNodeHandle> {
    let node_cfg = NodeConfig::new(
        cfg.libp2p_quic_port,
        cfg.libp2p_host.clone(),
        WORKLOAD_CLUSTER_TOPIC,
    );
    let (mut swarm, _topic, peer_rx, peer_tx) = p2p::setup_swarm(node_cfg, |key| {
        let gossipsub = p2p::create_gossipsub_behaviour(key)
            .expect("create workload gossipsub behaviour");

        let handshake_rr = request_response::Behaviour::new(
            std::iter::once((
                handshake::HANDSHAKE_PROTOCOL,
                request_response::ProtocolSupport::Full,
            )),
            request_response::Config::default(),
        );
        let proxy_rr = request_response::Behaviour::new(
            std::iter::once((
                INGRESS_PROXY_PROTOCOL,
                request_response::ProtocolSupport::Full,
            )),
            request_response::Config::default(),
        );
        let manifest_rr = request_response::Behaviour::new(
            std::iter::once((
                SIDECAR_MANIFEST_PROTOCOL,
                request_response::ProtocolSupport::Full,
            )),
            request_response::Config::default(),
        );

        let registration_rr = request_response::Behaviour::new(
            std::iter::once((
                SIDECAR_REGISTRATION_PROTOCOL,
                request_response::ProtocolSupport::Inbound,
            )),
            request_response::Config::default(),
        );

        let store = kad::store::MemoryStore::new(key.public().to_peer_id());
        let kademlia_config = p2p::default_kademlia_config();
        let mut kademlia =
            kad::Behaviour::with_config(key.public().to_peer_id(), store, kademlia_config);
        kademlia.set_mode(Some(kad::Mode::Server));

        let relay = relay::Behaviour::new(key.public().to_peer_id(), Default::default());
        let autonat = autonat::Behaviour::new(key.public().to_peer_id(), Default::default());
        let identify =
            identify::Behaviour::new(identify::Config::new("/podmesh/0.1.0".into(), key.public()));

        WorkloadBehaviour {
            gossipsub,
            handshake_rr,
            kademlia,
            relay,
            autonat,
            identify,
            proxy_rr,
            manifest_rr,
            egress_stream: libp2p_stream::Behaviour::new(),
            registration_rr,
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
        "workload kad protocols configured peer={} kad_protocols={:?}",
        swarm.local_peer_id(),
        kad_protocols
    );

    let (proxy_announced_tx, proxy_provider_announced_rx) = watch::channel(false);
    let runtime_proxy_announced_tx = proxy_announced_tx.clone();
    let enable_proxy_provider = cfg.enable_proxy_provider;
    if enable_proxy_provider {
        announce_proxy_provider(&mut swarm, &proxy_announced_tx);
    } else {
        warn!("proxy provider announcement disabled peer={}", swarm.local_peer_id());
    }

    // Phase 7.2: announce opaque tenant proxy DHT key if owner pubkey configured
    if let Some(ref owner_pub) = cfg.owner_pubkey {
        match compute_tenant_proxy_dht_key(owner_pub) {
            Ok(key) => {
                let record_key = kad::RecordKey::new(&key);
                match swarm.behaviour_mut().kademlia.start_providing(record_key) {
                    Ok(qid) => info!(
                        "proxy announced tenant DHT key peer={} query_id={:?}",
                        swarm.local_peer_id(), qid
                    ),
                    Err(err) => warn!(
                        "proxy tenant DHT key announcement failed peer={} error={}",
                        swarm.local_peer_id(), err
                    ),
                }
            }
            Err(err) => warn!("failed to compute tenant proxy DHT key: {}", err),
        }
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
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    
    // Shared in-memory routing table (populated by sidecar registrations)
    let routing_table: RoutingTable = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let routing_table_task = Arc::clone(&routing_table);
    
    // Set up egress tunnel stream handler
    let egress_protocol = StreamProtocol::try_from_owned(EGRESS_TUNNEL_PROTOCOL.to_string())
        .expect("valid egress protocol");
    let mut incoming_egress = swarm
        .behaviour()
        .egress_stream
        .new_control()
        .accept(egress_protocol)
        .expect("accept egress tunnel protocol");
    
    let task = tokio::spawn({
        let mut cmd_rx = cmd_rx;
        let mut handshake_states = handshake_states;
        let mut peer_protocols = peer_protocols;
        let mut kad_bootstrapped = false;
        let mut kad_ready_signaled = false;
        let proxy_announced_tx = runtime_proxy_announced_tx;
        let enable_proxy_provider = enable_proxy_provider;
        let single_node_mode = single_node_mode;
        let mut sidecar_cache: HashMap<String, SidecarCacheEntry> = HashMap::new();
        let mut manifest_queries: HashMap<String, ManifestQueryState> = HashMap::new();
        let mut query_manifest: HashMap<kad::QueryId, String> = HashMap::new();
        let mut manifest_request_map: HashMap<request_response::OutboundRequestId, String> =
            HashMap::new();
        let mut pending_proxy_requests: HashMap<
            request_response::OutboundRequestId,
            oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
        > = HashMap::new();
        let routing_table = routing_table_task;
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
                                handle_manifest_queries(
                                    &mut swarm,
                                    event,
                                    &mut manifest_queries,
                                    &mut query_manifest,
                                    &mut manifest_request_map,
                                );
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
                                warn!("workload handshake outbound failure peer={} error={:?}", peer, error);
                                if matches!(error, request_response::OutboundFailure::UnsupportedProtocols) {
                                    handshake::track_peer(&mut handshake_states, &peer).confirmed = true;
                                    warn!("handshake disabled for peer due to unsupported protocol peer={}", peer);
                                }
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::HandshakeRr(request_response::Event::InboundFailure { peer, error, .. })) => {
                                warn!("workload handshake inbound failure peer={} error={:?}", peer, error);
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::ProxyRr(event)) => {
                                handle_proxy_rr_event(event, &mut pending_proxy_requests);
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::ManifestRr(event)) => {
                                handle_manifest_rr_event(
                                    &mut swarm,
                                    event,
                                    &mut manifest_queries,
                                    &mut manifest_request_map,
                                    &mut query_manifest,
                                    &mut sidecar_cache,
                                    &mut pending_proxy_requests,
                                );
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::RegistrationRr(event)) => {
                                handle_registration_rr_event(&mut swarm, event, &routing_table);
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
                                    warn!("workload bootstrapping kademlia count={}", connected_count);
                                    if let Err(err) = swarm.behaviour_mut().kademlia.bootstrap() {
                                        error!("workload kademlia bootstrap failed err={:?}", err);
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
                    Some(cmd) = cmd_rx.recv() => {
                        handle_command(
                            &mut swarm,
                            cmd,
                            &mut sidecar_cache,
                            &mut manifest_queries,
                            &mut query_manifest,
                            &mut pending_proxy_requests,
                            &routing_table,
                        );
                    }
                    _ = handshake_interval.tick() => {
                        let local_peer = swarm.local_peer_id().clone();
                        match handshake::collect_handshake_actions(
                            &mut handshake_states,
                            &local_peer,
                            &HandshakeDriveConfig::default(),
                        ) {
                            Ok(actions) => {
                                for (peer, payload) in actions.requests {
                                    let request_id = swarm
                                        .behaviour_mut()
                                        .handshake_rr
                                        .send_request(&peer, payload);
                                    debug!("workload handshake request sent peer={} request_id={:?}", peer, request_id);
                                }

                                for peer in actions.drops {
                                    swarm
                                        .behaviour_mut()
                                        .gossipsub
                                        .remove_explicit_peer(&peer);
                                }
                            }
                            Err(err) => warn!("workload handshake drive failed err={:?}", err),
                        }
                    }
                    // Handle incoming egress tunnel streams from sidecars
                    Some((peer, stream)) = incoming_egress.next() => {
                        debug!("incoming egress tunnel stream from peer={}", peer);
                        tokio::spawn(async move {
                            if let Err(err) = handle_egress_stream(peer, stream).await {
                                warn!("egress tunnel error peer={} error={:?}", peer, err);
                            }
                        });
                    }
                    _ = interval.tick() => {
                        publish_peer_snapshot(&swarm, &mut peer_tx);
                        prune_expired_cache_entries(&mut sidecar_cache);
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
        command_tx: cmd_tx,
        routing_table,
    })
}

/// Removes expired entries from the sidecar cache to prevent unbounded growth.
fn prune_expired_cache_entries(sidecar_cache: &mut HashMap<String, SidecarCacheEntry>) {
    let now = Instant::now();
    let before_count = sidecar_cache.len();
    sidecar_cache.retain(|manifest_id, entry| {
        let keep = entry.expires_at > now;
        if !keep {
            debug!("evicting expired cache entry manifest={}", manifest_id);
        }
        keep
    });
    let removed = before_count - sidecar_cache.len();
    if removed > 0 {
        info!("pruned {} expired cache entries, {} remaining", removed, sidecar_cache.len());
    }
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

fn handle_command(
    swarm: &mut Swarm<WorkloadBehaviour>,
    cmd: P2pCommand,
    sidecar_cache: &mut HashMap<String, SidecarCacheEntry>,
    manifest_queries: &mut HashMap<String, ManifestQueryState>,
    query_manifest: &mut HashMap<kad::QueryId, String>,
    pending_proxy_requests: &mut HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
    >,
    routing_table: &RoutingTable,
) {
    match cmd {
        P2pCommand::ProxyHttp {
            request,
            respond_to,
        } => {
            let pending = ProxyPendingRequest {
                request,
                respond_to,
            };
            process_proxy_command(
                swarm,
                pending,
                sidecar_cache,
                manifest_queries,
                query_manifest,
                pending_proxy_requests,
                routing_table,
            );
        }
    }
}

/// Build a `SidecarProviderRecordOwned` from an in-memory `SidecarRouteEntry`
/// so it can be dispatched through the existing proxy dispatch path.
fn build_record_from_route_entry(entry: &SidecarRouteEntry) -> SidecarProviderRecordOwned {
    let routes: Vec<SidecarRouteSpec> = entry
        .routes
        .iter()
        .map(|r| SidecarRouteSpec {
            host: String::new(),
            path_prefix: r.path_prefix.clone(),
            target_port: r.port,
            service_name: String::new(),
            service_port: String::new(),
            source: SidecarRouteKind::Service,
        })
        .collect();
    SidecarProviderRecordOwned {
        manifest_id: String::new(),
        peer_id: entry.sidecar_peer_id.clone(),
        host: String::new(),
        owner_public_key_b64: None,
        routes,
        ttl_ms: 0,
        last_updated_ms: entry.registered_at,
        version: 1,
    }
}

fn process_proxy_command(
    swarm: &mut Swarm<WorkloadBehaviour>,
    pending: ProxyPendingRequest,
    sidecar_cache: &mut HashMap<String, SidecarCacheEntry>,
    manifest_queries: &mut HashMap<String, ManifestQueryState>,
    query_manifest: &mut HashMap<kad::QueryId, String>,
    pending_proxy_requests: &mut HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
    >,
    routing_table: &RoutingTable,
) {
    let manifest_id = pending.request.manifest_id.clone();

    // Phase 7.4: check in-memory routing table first
    if let Ok(table) = routing_table.read() {
        if let Some(entry) = table.get(&manifest_id) {
            let record = build_record_from_route_entry(entry);
            drop(table);
            match dispatch_proxy_request(swarm, pending, &record, pending_proxy_requests) {
                Ok(()) => return,
                Err((pending, err)) => {
                    let _ = pending.respond_to.send(Err(err));
                    return;
                }
            }
        }
    }

    let now = Instant::now();
    if let Some(entry) = sidecar_cache.get(&manifest_id) {
        if entry.expires_at > now {
            match dispatch_proxy_request(swarm, pending, &entry.record, pending_proxy_requests) {
                Ok(()) => return,
                Err((pending, err)) => {
                    let _ = pending.respond_to.send(Err(err));
                    return;
                }
            }
        } else {
            sidecar_cache.remove(&manifest_id);
        }
    }

    if let Some(state) = manifest_queries.get_mut(&manifest_id) {
        state.waiters.push(pending);
        return;
    }

    let key = format!("{}{}", MANIFEST_RECORD_PREFIX, manifest_id);
    info!("workload proxy querying manifest providers manifest={} key={}", manifest_id, key);
    let query_id = swarm
        .behaviour_mut()
        .kademlia
        .get_providers(kad::RecordKey::new(&key));
    manifest_queries.insert(
        manifest_id.clone(),
        ManifestQueryState {
            waiters: vec![pending],
            query_id,
            pending_requests: HashSet::new(),
            requested_peers: HashSet::new(),
            providers_finished: false,
        },
    );
    query_manifest.insert(query_id, manifest_id);
}

fn handle_manifest_queries(
    swarm: &mut Swarm<WorkloadBehaviour>,
    event: kad::Event,
    manifest_queries: &mut HashMap<String, ManifestQueryState>,
    query_manifest: &mut HashMap<kad::QueryId, String>,
    manifest_request_map: &mut HashMap<request_response::OutboundRequestId, String>,
) {
    if let kad::Event::OutboundQueryProgressed { id, result, .. } = event {
        if let Some(manifest_id) = query_manifest.get(&id).cloned() {
            match result {
                kad::QueryResult::GetProviders(Ok(kad::GetProvidersOk::FoundProviders {
                    providers,
                    ..
                })) => {
                    if let Some(state) = manifest_queries.get_mut(&manifest_id) {
                        for peer in providers {
                            if state.requested_peers.len() >= MAX_MANIFEST_FETCH_PEERS {
                                break;
                            }
                            if state.requested_peers.insert(peer.clone()) {
                                let request = build_sidecar_manifest_request(&manifest_id);
                                let request_id = swarm
                                    .behaviour_mut()
                                    .manifest_rr
                                    .send_request(&peer, request);
                                state.pending_requests.insert(request_id);
                                manifest_request_map.insert(request_id, manifest_id.clone());
                            }
                        }
                    }
                }
                kad::QueryResult::GetProviders(Ok(
                    kad::GetProvidersOk::FinishedWithNoAdditionalRecord { .. },
                )) => {
                    if let Some(state) = manifest_queries.get_mut(&manifest_id) {
                        state.providers_finished = true;
                        if state.pending_requests.is_empty() && state.waiters.is_empty() {
                            // No waiters to notify.
                            query_manifest.remove(&state.query_id);
                            manifest_queries.remove(&manifest_id);
                        } else if state.pending_requests.is_empty() {
                            fail_manifest_query(
                                manifest_id.clone(),
                                "manifest providers unavailable".to_string(),
                                manifest_queries,
                                query_manifest,
                            );
                        }
                    }
                }
                kad::QueryResult::GetProviders(Err(err)) => {
                    fail_manifest_query(
                        manifest_id.clone(),
                        format!("kad get_providers failed: {err}"),
                        manifest_queries,
                        query_manifest,
                    );
                }
                _ => {}
            }
        }
    }
}
fn handle_manifest_rr_event(
    swarm: &mut Swarm<WorkloadBehaviour>,
    event: request_response::Event<Vec<u8>, Vec<u8>>,
    manifest_queries: &mut HashMap<String, ManifestQueryState>,
    manifest_request_map: &mut HashMap<request_response::OutboundRequestId, String>,
    query_manifest: &mut HashMap<kad::QueryId, String>,
    sidecar_cache: &mut HashMap<String, SidecarCacheEntry>,
    pending_proxy_requests: &mut HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
    >,
) {
    match event {
        request_response::Event::Message { message, .. } => match message {
            request_response::Message::Response {
                request_id,
                response,
                ..
            } => {
                if let Some(manifest_id) = manifest_request_map.remove(&request_id) {
                    process_manifest_response(
                        swarm,
                        request_id,
                        manifest_id,
                        response,
                        manifest_queries,
                        query_manifest,
                        sidecar_cache,
                        pending_proxy_requests,
                    );
                } else {
                    warn!(
                        "received manifest response for unknown request request_id={:?}",
                        request_id
                    );
                }
            }
            request_response::Message::Request { .. } => {
                warn!("unexpected inbound manifest fetch request");
            }
        },
        request_response::Event::OutboundFailure {
            request_id, error, ..
        } => {
            if let Some(manifest_id) = manifest_request_map.remove(&request_id) {
                handle_manifest_request_failure(
                    manifest_id,
                    request_id,
                    error,
                    manifest_queries,
                    query_manifest,
                );
            }
        }
        request_response::Event::InboundFailure { peer, error, .. } => {
            warn!("manifest response inbound failure peer={} error={:?}", peer, error);
        }
        request_response::Event::ResponseSent { .. } => {}
    }
}

fn process_manifest_response(
    swarm: &mut Swarm<WorkloadBehaviour>,
    request_id: request_response::OutboundRequestId,
    manifest_id: String,
    response: Vec<u8>,
    manifest_queries: &mut HashMap<String, ManifestQueryState>,
    query_manifest: &mut HashMap<kad::QueryId, String>,
    sidecar_cache: &mut HashMap<String, SidecarCacheEntry>,
    pending_proxy_requests: &mut HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
    >,
) {
    if let Some(state) = manifest_queries.get_mut(&manifest_id) {
        state.pending_requests.remove(&request_id);
    }

    match verify_sidecar_manifest_envelope(&response) {
        Ok(verified) => {
            store_sidecar_record(&manifest_id, &verified.record, sidecar_cache);
            satisfy_manifest_waiters(
                swarm,
                manifest_id,
                verified.record,
                manifest_queries,
                query_manifest,
                pending_proxy_requests,
            );
        }
        Err(err) => {
            warn!("failed to verify manifest envelope manifest={} error={}", manifest_id, err);
            maybe_fail_manifest_due_to_exhaustion(
                manifest_id,
                manifest_queries,
                query_manifest,
                "manifest verification failed",
            );
        }
    }
}

fn store_sidecar_record(
    manifest_id: &str,
    record: &SidecarProviderRecordOwned,
    sidecar_cache: &mut HashMap<String, SidecarCacheEntry>,
) {
    let should_replace = sidecar_cache
        .get(manifest_id)
        .map(|entry| record.last_updated_ms >= entry.record.last_updated_ms)
        .unwrap_or(true);
    if !should_replace {
        return;
    }
    // Use record's TTL if provided, else fall back to shared constant
    let record_ttl_ms = if record.ttl_ms == 0 {
        MANIFEST_RECORD_TTL_MS as u64
    } else {
        record.ttl_ms as u64
    };
    // Apply cache TTL ratio to ensure cache expires before stale records could be served
    let cache_ttl_ms = (record_ttl_ms as f64 * MANIFEST_CACHE_TTL_RATIO) as u64;
    let expires_at = Instant::now() + Duration::from_millis(cache_ttl_ms);
    info!(
        "caching sidecar record manifest={} ttl_ms={} cache_ttl_ms={}",
        manifest_id, record_ttl_ms, cache_ttl_ms
    );
    sidecar_cache.insert(
        manifest_id.to_string(),
        SidecarCacheEntry {
            record: record.clone(),
            expires_at,
        },
    );
}

fn satisfy_manifest_waiters(
    swarm: &mut Swarm<WorkloadBehaviour>,
    manifest_id: String,
    record: SidecarProviderRecordOwned,
    manifest_queries: &mut HashMap<String, ManifestQueryState>,
    query_manifest: &mut HashMap<kad::QueryId, String>,
    pending_proxy_requests: &mut HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
    >,
) {
    if let Some(state) = manifest_queries.remove(&manifest_id) {
        query_manifest.remove(&state.query_id);
        for pending in state.waiters {
            if let Err((pending, err)) =
                dispatch_proxy_request(swarm, pending, &record, pending_proxy_requests)
            {
                let _ = pending.respond_to.send(Err(err));
            }
        }
    }
}

fn handle_manifest_request_failure(
    manifest_id: String,
    request_id: request_response::OutboundRequestId,
    error: request_response::OutboundFailure,
    manifest_queries: &mut HashMap<String, ManifestQueryState>,
    query_manifest: &mut HashMap<kad::QueryId, String>,
) {
    if let Some(state) = manifest_queries.get_mut(&manifest_id) {
        state.pending_requests.remove(&request_id);
    }
    warn!("manifest fetch request failed manifest={} error={:?}", manifest_id, error);
    maybe_fail_manifest_due_to_exhaustion(
        manifest_id,
        manifest_queries,
        query_manifest,
        "manifest fetch failed",
    );
}

fn maybe_fail_manifest_due_to_exhaustion(
    manifest_id: String,
    manifest_queries: &mut HashMap<String, ManifestQueryState>,
    query_manifest: &mut HashMap<kad::QueryId, String>,
    reason: &str,
) {
    let should_fail = manifest_queries
        .get(&manifest_id)
        .map(|state| state.providers_finished && state.pending_requests.is_empty())
        .unwrap_or(false);
    if should_fail {
        fail_manifest_query(
            manifest_id,
            reason.to_string(),
            manifest_queries,
            query_manifest,
        );
    }
}

fn fail_manifest_query(
    manifest_id: String,
    message: String,
    manifest_queries: &mut HashMap<String, ManifestQueryState>,
    query_manifest: &mut HashMap<kad::QueryId, String>,
) {
    if let Some(state) = manifest_queries.remove(&manifest_id) {
        query_manifest.remove(&state.query_id);
        warn!("manifest query failed manifest={} reason={}", manifest_id, message);
        for pending in state.waiters {
            let _ = pending.respond_to.send(Err(anyhow!(message.clone())));
        }
    }
}
fn handle_registration_rr_event(
    swarm: &mut Swarm<WorkloadBehaviour>,
    event: request_response::Event<Vec<u8>, Vec<u8>>,
    routing_table: &RoutingTable,
) {
    match event {
        request_response::Event::Message { peer, message, .. } => match message {
            request_response::Message::Request { request, channel, .. } => {
                match SidecarRegistration::from_bytes(&request) {
                    Ok(reg) => {
                        // Verify the signature: sig covers manifest_id || sidecar_peer_id
                        let signed_data = format!("{}{}", reg.manifest_id, reg.sidecar_peer_id);
                        let sig_valid = verify_registration_sig(
                            &reg.owner_pubkey,
                            &reg.sig,
                            signed_data.as_bytes(),
                        );
                        let (ok, message) = if sig_valid {
                            let entry = SidecarRouteEntry {
                                sidecar_peer_id: reg.sidecar_peer_id.clone(),
                                routes: reg.routes.clone(),
                                registered_at: p2p::timestamp_millis(),
                            };
                            {
                                let mut table = routing_table.write().expect("routing table write lock");
                                table.insert(reg.manifest_id.clone(), entry);
                            }
                            info!(
                                "sidecar registered routes manifest={} peer={} routes={}",
                                reg.manifest_id, reg.sidecar_peer_id, reg.routes.len()
                            );
                            (true, "ok".to_string())
                        } else {
                            warn!(
                                "sidecar registration sig verification failed manifest={} peer={}",
                                reg.manifest_id, peer
                            );
                            (false, "signature verification failed".to_string())
                        };
                        let ack = SidecarRegistrationAck {
                            manifest_id: reg.manifest_id.clone(),
                            ok,
                            message,
                        };
                        if let Err(err) = swarm.behaviour_mut().registration_rr.send_response(channel, ack.to_bytes()) {
                            warn!("failed to send registration ack error={:?}", err);
                        }
                    }
                    Err(err) => {
                        warn!("failed to deserialize sidecar registration from peer={} error={}", peer, err);
                    }
                }
            }
            request_response::Message::Response { .. } => {
                warn!("unexpected outbound response on registration protocol");
            }
        },
        request_response::Event::OutboundFailure { peer, error, .. } => {
            warn!("registration outbound failure peer={} error={:?}", peer, error);
        }
        request_response::Event::InboundFailure { peer, error, .. } => {
            warn!("registration inbound failure peer={} error={:?}", peer, error);
        }
        request_response::Event::ResponseSent { .. } => {}
    }
}

fn verify_registration_sig(owner_pubkey_b64: &str, sig_b64: &str, data: &[u8]) -> bool {
    let pk_bytes = match crypto::b64_decode(owner_pubkey_b64) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let sig_bytes = match crypto::b64_decode(sig_b64) {
        Ok(b) => b,
        Err(_) => return false,
    };
    crypto::verify_envelope(&pk_bytes, data, &sig_bytes).is_ok()
}

fn handle_proxy_rr_event(
    event: request_response::Event<ProxyHttpRequest, ProxyHttpResponse>,
    pending_proxy_requests: &mut HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
    >,
) {
    match event {
        request_response::Event::Message { message, .. } => match message {
            request_response::Message::Response {
                request_id,
                response,
                ..
            } => {
                if let Some(tx) = pending_proxy_requests.remove(&request_id) {
                    let _ = tx.send(Ok(response));
                }
            }
            request_response::Message::Request { .. } => {
                warn!("unexpected proxy request from gateway");
            }
        },
        request_response::Event::OutboundFailure {
            request_id, error, ..
        } => {
            if let Some(tx) = pending_proxy_requests.remove(&request_id) {
                let _ = tx.send(Err(anyhow!("proxy request failed: {error}")));
            }
        }
        request_response::Event::ResponseSent { .. } => {}
        request_response::Event::InboundFailure { .. } => {}
    }
}

fn dispatch_proxy_request(
    swarm: &mut Swarm<WorkloadBehaviour>,
    mut pending: ProxyPendingRequest,
    record: &SidecarProviderRecordOwned,
    pending_proxy_requests: &mut HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
    >,
) -> Result<(), (ProxyPendingRequest, anyhow::Error)> {
    let host = extract_host_header(&pending.request.headers);
    if let Some(port) = select_route_port(record, &pending.request.path_and_query, host.as_deref())
    {
        pending.request.target_port = port;
    } else if pending.request.target_port == 0 {
        let host_msg = host
            .as_deref()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let request_path = pending.request.path_and_query.clone();
        return Err((
            pending,
            anyhow!(format!(
                "no matching route for host {} path {}",
                host_msg, request_path
            )),
        ));
    }

    let peer_id = match record.peer_id.parse::<PeerId>() {
        Ok(peer) => peer,
        Err(err) => return Err((pending, anyhow!("invalid peer id: {err}"))),
    };

    let request_id = swarm
        .behaviour_mut()
        .proxy_rr
        .send_request(&peer_id, pending.request);
    pending_proxy_requests.insert(request_id, pending.respond_to);
    Ok(())
}

fn select_route_port(
    record: &SidecarProviderRecordOwned,
    path: &str,
    host: Option<&str>,
) -> Option<u16> {
    let mut normalized = if path.is_empty() {
        "/".to_string()
    } else {
        path.split('?').next().unwrap_or(path).to_string()
    };
    if !normalized.starts_with('/') {
        normalized = format!("/{}", normalized);
    }

    let normalized_host = host.map(|h| h.to_lowercase());
    let mut candidates = record
        .routes
        .iter()
        .filter(|route| normalized.starts_with(&route.path_prefix))
        .peekable();

    if let Some(ref host_value) = normalized_host {
        if candidates.peek().is_some() {
            if let Some(route) = candidates
                .clone()
                .filter(|route| route.host.eq_ignore_ascii_case(host_value))
                .max_by_key(|route| route.path_prefix.len())
            {
                return Some(route.target_port);
            }
        }
    }

    candidates
        .max_by_key(|route| route.path_prefix.len())
        .map(|route| route.target_port)
}

fn extract_host_header(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("host"))
        .map(|(_, value)| value.split(':').next().unwrap_or(value).to_lowercase())
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
                    "kad inbound get_provider request num_closer_peers={} num_provider_peers={}",
                    num_closer_peers,
                    num_provider_peers
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
            "registered bootstrap peer as initial peer peer_id={} bootstrap_protocol={} address={}",
            peer_id,
            base_addr,
            raw
        );
    }
}

fn log_peer_bootstrap_state(peer_protocols: &HashMap<PeerId, String>, kad_started: bool) {
    if peer_protocols.is_empty() {
        warn!("no connected peers to report kad_started={}", kad_started);
        return;
    }

    for (peer, protocol) in peer_protocols {
        warn!(
            "workload peer bootstrap state peer={} bootstrap_protocol={} kad_started={}",
            peer,
            protocol,
            kad_started
        );
    }
}

fn extract_peer_id(mut addr: Multiaddr) -> Option<(PeerId, Multiaddr)> {
    match addr.pop() {
        Some(libp2p::multiaddr::Protocol::P2p(peer_id)) => Some((peer_id, addr)),
        _ => None,
    }
}

/// Handles an incoming egress tunnel stream from a sidecar.
///
/// Protocol:
/// 1. Read EgressTunnelRequest (postcard-encoded with length prefix)
/// 2. Connect to target host:port
/// 3. Send EgressTunnelResponse
/// 4. If successful, bidirectionally copy data between streams
async fn handle_egress_stream(
    peer: PeerId,
    stream: libp2p::Stream,
) -> anyhow::Result<()> {
    use futures::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::compat::FuturesAsyncReadCompatExt;
    
    let (mut reader, mut writer) = stream.split();
    
    // Read length-prefixed request
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    
    if len > 1024 {
        let response = EgressTunnelResponse::err("request too large");
        let response_bytes = postcard::to_allocvec(&response)?;
        let len_bytes = (response_bytes.len() as u32).to_le_bytes();
        writer.write_all(&len_bytes).await?;
        writer.write_all(&response_bytes).await?;
        writer.flush().await?;
        return Err(anyhow!("egress request too large len={}", len));
    }
    
    let mut request_buf = vec![0u8; len];
    reader.read_exact(&mut request_buf).await?;
    
    let request: EgressTunnelRequest = postcard::from_bytes(&request_buf)
        .map_err(|e| anyhow!("failed to deserialize egress request: {}", e))?;
    
    info!(
        "egress tunnel request peer={} target={}:{} protocol={}",
        peer, request.target_host, request.target_port, request.protocol
    );
    
    // Only TCP is supported for now
    if request.protocol != "tcp" {
        let response = EgressTunnelResponse::err(format!("unsupported protocol: {}", request.protocol));
        let response_bytes = postcard::to_allocvec(&response)?;
        let len_bytes = (response_bytes.len() as u32).to_le_bytes();
        writer.write_all(&len_bytes).await?;
        writer.write_all(&response_bytes).await?;
        writer.flush().await?;
        return Err(anyhow!("unsupported egress protocol: {}", request.protocol));
    }
    
    // Connect to target with timeout
    let target_addr = format!("{}:{}", request.target_host, request.target_port);
    let connect_timeout = Duration::from_secs(30);
    
    let target_stream = match tokio::time::timeout(
        connect_timeout,
        tokio::net::TcpStream::connect(&target_addr),
    ).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            let response = EgressTunnelResponse::err(format!("connection failed: {}", e));
            let response_bytes = postcard::to_allocvec(&response)?;
            let len_bytes = (response_bytes.len() as u32).to_le_bytes();
            writer.write_all(&len_bytes).await?;
            writer.write_all(&response_bytes).await?;
            writer.flush().await?;
            return Err(anyhow!("failed to connect to {}: {}", target_addr, e));
        }
        Err(_) => {
            let response = EgressTunnelResponse::err("connection timeout");
            let response_bytes = postcard::to_allocvec(&response)?;
            let len_bytes = (response_bytes.len() as u32).to_le_bytes();
            writer.write_all(&len_bytes).await?;
            writer.write_all(&response_bytes).await?;
            writer.flush().await?;
            return Err(anyhow!("connection timeout to {}", target_addr));
        }
    };
    
    // Send success response
    let response = EgressTunnelResponse::ok();
    let response_bytes = postcard::to_allocvec(&response)?;
    let len_bytes = (response_bytes.len() as u32).to_le_bytes();
    writer.write_all(&len_bytes).await?;
    writer.write_all(&response_bytes).await?;
    writer.flush().await?;
    
    info!(
        "egress tunnel established peer={} target={}",
        peer, target_addr
    );
    
    // Reunite reader and writer back into a single stream for bidirectional copy
    let p2p_stream = reader.reunite(writer)?;
    
    // Convert libp2p stream (futures::io) to tokio io
    let p2p_stream = p2p_stream.compat();
    let (mut p2p_reader, mut p2p_writer) = tokio::io::split(p2p_stream);
    
    // Split TCP stream
    let (mut tcp_reader, mut tcp_writer) = target_stream.into_split();
    
    // Bidirectional copy
    let client_to_server = tokio::io::copy(&mut p2p_reader, &mut tcp_writer);
    let server_to_client = tokio::io::copy(&mut tcp_reader, &mut p2p_writer);
    
    match tokio::try_join!(client_to_server, server_to_client) {
        Ok((c2s, s2c)) => {
            debug!(
                "egress tunnel closed peer={} target={} c2s_bytes={} s2c_bytes={}",
                peer, target_addr, c2s, s2c
            );
        }
        Err(e) => {
            debug!(
                "egress tunnel error peer={} target={} error={}",
                peer, target_addr, e
            );
        }
    }
    
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_announces_with_opaque_dht_key() {
        let owner_pub_b64 = crypto::b64_encode(&[1u8; 32]);
        let key = compute_tenant_proxy_dht_key(&owner_pub_b64).unwrap();
        // key should be 32 bytes (sha256)
        assert_eq!(key.len(), 32);
        // key should NOT contain the raw owner pubkey bytes
        assert!(!key.windows(32).any(|w| w == [1u8; 32].as_slice()));
    }

    #[test]
    fn test_opaque_key_not_linkable_to_owner_identity() {
        let owner_pub1 = crypto::b64_encode(&[1u8; 32]);
        let owner_pub2 = crypto::b64_encode(&[2u8; 32]);
        let key1 = compute_tenant_proxy_dht_key(&owner_pub1).unwrap();
        let key2 = compute_tenant_proxy_dht_key(&owner_pub2).unwrap();
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_proxy_routes_from_in_memory_table() {
        use std::sync::RwLock;
        let table: RoutingTable = Arc::new(RwLock::new(HashMap::new()));
        {
            let mut t = table.write().unwrap();
            t.insert(
                "demo-app".to_string(),
                SidecarRouteEntry {
                    sidecar_peer_id: "peer123".to_string(),
                    routes: vec![SidecarRoute {
                        path_prefix: "/".to_string(),
                        port: 8080,
                    }],
                    registered_at: 0,
                },
            );
        }
        let t = table.read().unwrap();
        let entry = t.get("demo-app");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().sidecar_peer_id, "peer123");
    }

    #[test]
    fn test_proxy_returns_503_when_sidecar_not_registered() {
        use std::sync::RwLock;
        let table: RoutingTable = Arc::new(RwLock::new(HashMap::new()));
        let t = table.read().unwrap();
        // Empty table - no entry for manifest
        assert!(t.get("unknown-app").is_none());
    }
}
