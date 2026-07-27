use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, autonat, gossipsub, identify, relay,
    request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use log::{debug, info, warn};
pub use p2p::handshake;
use p2p::handshake::{HandshakeDriveConfig, HandshakeState, ProxyCertProvider};
use p2p::{
    CoreBehaviourAccess, NodeConfig,
    http_proxy::{ProxyCodec, ProxyHttpRequest, ProxyHttpResponse},
    libp2p_stream,
    request_response::{ByteCodec, HandshakeCodec},
};
use protocol::egress::{EgressTunnelRequest, EgressTunnelResponse};
use protocol::libp2p_constants::{
    EGRESS_TUNNEL_PROTOCOL, INGRESS_PROXY_PROTOCOL, PROXY_DISCOVERY_PROTOCOL,
    SIDECAR_REGISTRATION_PROTOCOL, WORKLOAD_CLUSTER_TOPIC,
};
use protocol::machine::{SidecarRouteKind, SidecarRouteSpec};
use protocol::{
    ProxyDiscoveryRequest, ProxyDiscoveryResponse, ProxyPeer, SidecarRegistration,
    SidecarRegistrationAck, SidecarRoute,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::restapi::{CertStore, new_cert_store};

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
const MAX_REGISTERED_SIDECARS: usize = 10_000;
const SIDECAR_REGISTRATION_TTL_MS: u64 = 120_000;

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

struct DispatchRecord {
    peer_id: String,
    routes: Vec<SidecarRouteSpec>,
}

#[derive(NetworkBehaviour)]
pub struct WorkloadBehaviour {
    pub gossipsub: gossipsub::Behaviour,
    pub handshake_rr: request_response::Behaviour<HandshakeCodec>,
    pub relay: relay::Behaviour,
    pub autonat: autonat::Behaviour,
    pub identify: identify::Behaviour,
    pub proxy_rr: request_response::Behaviour<ProxyCodec>,
    pub egress_stream: libp2p_stream::Behaviour,
    pub registration_rr: request_response::Behaviour<RegistrationCodec>,
    pub discovery_rr: request_response::Behaviour<ByteCodec>,
}

impl CoreBehaviourAccess for WorkloadBehaviour {
    fn gossipsub_mut(&mut self) -> &mut gossipsub::Behaviour {
        &mut self.gossipsub
    }
}

pub struct P2pNodeHandle {
    task: JoinHandle<()>,
    peer_rx: watch::Receiver<Vec<String>>,
    peer_id: String,
    network_ready_rx: watch::Receiver<bool>,
    command_tx: mpsc::UnboundedSender<P2pCommand>,
    pub routing_table: RoutingTable,
    /// Shared store of tenant-issued NodeCerts. The REST API writes here when
    /// `POST /api/v1/node_cert` is called; the p2p task reads it for SidecarRegistration verification.
    pub cert_store: CertStore,
}

impl P2pNodeHandle {
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    pub fn peer_rx(&self) -> watch::Receiver<Vec<String>> {
        self.peer_rx.clone()
    }

    pub fn network_ready_rx(&self) -> watch::Receiver<bool> {
        self.network_ready_rx.clone()
    }

    pub fn proxy_client(&self) -> ProxyClient {
        ProxyClient {
            tx: self.command_tx.clone(),
        }
    }

    pub fn cert_store(&self) -> CertStore {
        self.cert_store.clone()
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
    let identity = cfg.identity.load()?;
    let node_cfg = NodeConfig::new(
        cfg.libp2p_quic_port,
        cfg.libp2p_host.clone(),
        WORKLOAD_CLUSTER_TOPIC,
    );
    let (mut swarm, _topic, peer_rx, peer_tx) = p2p::setup_swarm(node_cfg, identity, |key| {
        let gossipsub =
            p2p::create_gossipsub_behaviour(key).expect("create workload gossipsub behaviour");

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
        let registration_rr = request_response::Behaviour::new(
            std::iter::once((
                SIDECAR_REGISTRATION_PROTOCOL,
                request_response::ProtocolSupport::Inbound,
            )),
            request_response::Config::default(),
        );
        let discovery_rr = request_response::Behaviour::new(
            std::iter::once((
                PROXY_DISCOVERY_PROTOCOL,
                request_response::ProtocolSupport::Inbound,
            )),
            request_response::Config::default(),
        );

        let relay = relay::Behaviour::new(key.public().to_peer_id(), Default::default());
        let autonat = autonat::Behaviour::new(key.public().to_peer_id(), Default::default());
        let identify =
            identify::Behaviour::new(identify::Config::new("/podmesh/0.1.0".into(), key.public()));

        WorkloadBehaviour {
            gossipsub,
            handshake_rr,
            relay,
            autonat,
            identify,
            proxy_rr,
            egress_stream: libp2p_stream::Behaviour::new(),
            registration_rr,
            discovery_rr,
        }
    })?;

    // Tenant cert plumbing — shared between REST API and p2p loop
    let cert_store: CertStore = new_cert_store();
    let handshake_cert_slot: ProxyCertProvider = cert_store.clone();

    let handshake_states: HashMap<PeerId, HandshakeState> = HashMap::new();
    let mut known_proxy_peers: HashMap<PeerId, ProxyPeer> = HashMap::new();

    for addr in &cfg.proxy_peer_multiaddrs {
        match addr.parse::<Multiaddr>() {
            Ok(ma) => {
                if let Some(peer) = proxy_peer_from_multiaddr(&ma) {
                    if let Ok(peer_id) = peer.peer_id.parse::<PeerId>() {
                        known_proxy_peers.insert(peer_id, peer);
                        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                    }
                }
                if let Err(err) = swarm.dial(ma) {
                    warn!("failed to dial proxy peer {}: {}", addr, err);
                }
            }
            Err(err) => warn!("invalid proxy peer multiaddr {}: {}", addr, err),
        }
    }

    let (network_ready_tx, network_ready_rx) = watch::channel(false);
    let mut peer_tx = peer_tx;
    let local_peer_id = swarm.local_peer_id().to_string();
    let network_ready_tx = network_ready_tx;
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    // Shared in-memory routing table (populated by sidecar registrations)
    let routing_table: RoutingTable = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let routing_table_task = Arc::clone(&routing_table);
    let cert_store_task = cert_store.clone();
    let handshake_cert_slot_task = handshake_cert_slot.clone();

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
        let mut known_proxy_peers = known_proxy_peers;
        let mut pending_proxy_requests: HashMap<
            request_response::OutboundRequestId,
            oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
        > = HashMap::new();
        let routing_table = routing_table_task;
        let cert_store = cert_store_task;
        let handshake_cert_slot = handshake_cert_slot_task;
        async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            let mut handshake_interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    event = swarm.select_next_some() => {
                        match event {
                            SwarmEvent::NewListenAddr { address, .. } => {
                                info!("workload libp2p listening on {}", address);
                                let _ = network_ready_tx.send(true);
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::Gossipsub(gossipsub::Event::Subscribed { peer_id, .. })) => {
                                warn!("peer {} subscribed", peer_id);
                                publish_peer_snapshot(&swarm, &mut peer_tx);
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::Gossipsub(gossipsub::Event::Unsubscribed { peer_id, .. })) => {
                                warn!("peer {} unsubscribed", peer_id);
                                publish_peer_snapshot(&swarm, &mut peer_tx);
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::HandshakeRr(request_response::Event::Message { message, peer, connection_id: _ })) => {
                                handshake::handle_request_response_message_with_cert(
                                    message,
                                    peer,
                                    &mut handshake_states,
                                    Some(&handshake_cert_slot),
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
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::RegistrationRr(event)) => {
                                handle_registration_rr_event(&mut swarm, event, &routing_table, &cert_store);
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::DiscoveryRr(event)) => {
                                handle_discovery_rr_event(&mut swarm, event, &known_proxy_peers, &cert_store);
                            }
                            SwarmEvent::Behaviour(WorkloadBehaviourEvent::Identify(
                                identify::Event::Received { peer_id, info, .. },
                            )) => {
                                if let Some(candidate) = proxy_peer_from_listen_addrs(peer_id, info.listen_addrs) {
                                    known_proxy_peers.insert(peer_id, candidate);
                                }
                            }
                            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                                warn!("connection established with {}", peer_id);
                                handshake::track_peer(&mut handshake_states, &peer_id);
                            }
                            SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
                                warn!("connection closed with {}", peer_id);
                                handshake::untrack_peer(&mut handshake_states, &peer_id);
                                let _ = num_established;
                            }
                            _ => {}
                        }
                    }
                    Some(cmd) = cmd_rx.recv() => {
                        handle_command(
                            &mut swarm,
                            cmd,
                            &mut pending_proxy_requests,
                            &routing_table,
                        );
                    }
                    _ = handshake_interval.tick() => {
                        let local_peer = *swarm.local_peer_id();
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
                        prune_stale_routes(&routing_table);
                    }
                }
            }
        }
    });

    Ok(P2pNodeHandle {
        task,
        peer_rx,
        peer_id: local_peer_id,
        network_ready_rx,
        command_tx: cmd_tx,
        routing_table,
        cert_store,
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

fn prune_stale_routes(routing_table: &RoutingTable) {
    let now = p2p::timestamp_millis();
    if let Ok(mut table) = routing_table.write() {
        table.retain(|_, entry| {
            now.saturating_sub(entry.registered_at) <= SIDECAR_REGISTRATION_TTL_MS
        });
    }
}

fn handle_command(
    swarm: &mut Swarm<WorkloadBehaviour>,
    cmd: P2pCommand,
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
            process_proxy_command(swarm, pending, pending_proxy_requests, routing_table);
        }
    }
}

fn build_record_from_route_entry(entry: &SidecarRouteEntry) -> DispatchRecord {
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
    DispatchRecord {
        peer_id: entry.sidecar_peer_id.clone(),
        routes,
    }
}

fn process_proxy_command(
    swarm: &mut Swarm<WorkloadBehaviour>,
    pending: ProxyPendingRequest,
    pending_proxy_requests: &mut HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
    >,
    routing_table: &RoutingTable,
) {
    let manifest_id = pending.request.manifest_id.clone();

    if let Ok(table) = routing_table.read()
        && let Some(entry) = table.get(&manifest_id)
    {
        let record = build_record_from_route_entry(entry);
        drop(table);
        match dispatch_proxy_request(swarm, pending, &record, pending_proxy_requests) {
            Ok(()) => return,
            Err(error) => {
                let (pending, err) = *error;
                let _ = pending.respond_to.send(Err(err));
                return;
            }
        }
    }
    let _ = pending.respond_to.send(Err(anyhow!(format!(
        "manifest {} has no registered sidecar route",
        manifest_id
    ))));
}

fn proxy_peer_from_multiaddr(address: &Multiaddr) -> Option<ProxyPeer> {
    let (peer_id, _) = extract_peer_id(address.clone())?;
    let peer = ProxyPeer {
        peer_id: peer_id.to_string(),
        addresses: vec![address.to_string()],
    };
    peer.validate().ok()?;
    Some(peer)
}

fn proxy_peer_from_observed_addr(peer_id: PeerId, mut address: Multiaddr) -> Option<ProxyPeer> {
    if !matches!(
        address.iter().last(),
        Some(libp2p::multiaddr::Protocol::P2p(_))
    ) {
        address.push(libp2p::multiaddr::Protocol::P2p(peer_id));
    }
    proxy_peer_from_multiaddr(&address)
}

fn proxy_peer_from_listen_addrs(peer_id: PeerId, addresses: Vec<Multiaddr>) -> Option<ProxyPeer> {
    let addresses: Vec<String> = addresses
        .into_iter()
        .take(protocol::proxy_discovery::MAX_PROXY_ADDRS_PER_PEER)
        .filter_map(|address| proxy_peer_from_observed_addr(peer_id, address))
        .flat_map(|peer| peer.addresses)
        .collect();
    let peer = ProxyPeer {
        peer_id: peer_id.to_string(),
        addresses,
    };
    peer.validate().ok()?;
    Some(peer)
}

fn handle_discovery_rr_event(
    swarm: &mut Swarm<WorkloadBehaviour>,
    event: request_response::Event<Vec<u8>, Vec<u8>>,
    known_peers: &HashMap<PeerId, ProxyPeer>,
    cert_store: &CertStore,
) {
    if let request_response::Event::Message {
        peer,
        message: request_response::Message::Request {
            request, channel, ..
        },
        ..
    } = event
    {
        let response = build_discovery_response(&request, peer, known_peers, cert_store);
        if let Ok(bytes) = response.to_bytes()
            && let Err(err) = swarm
                .behaviour_mut()
                .discovery_rr
                .send_response(channel, bytes)
        {
            warn!("failed to send proxy discovery response error={:?}", err);
        }
    }
}

fn build_discovery_response(
    request_bytes: &[u8],
    requester: PeerId,
    known_peers: &HashMap<PeerId, ProxyPeer>,
    cert_store: &CertStore,
) -> ProxyDiscoveryResponse {
    match ProxyDiscoveryRequest::from_bytes(request_bytes) {
        Ok(request) if tenant_cert_is_valid(cert_store, &request.owner_pubkey) => {
            let peers = known_peers
                .iter()
                .filter(|(candidate, _)| **candidate != requester)
                .take(usize::from(request.limit))
                .map(|(_, candidate)| candidate.clone())
                .collect();
            ProxyDiscoveryResponse { peers }
        }
        Ok(_) => {
            warn!("proxy discovery rejected for unauthorized tenant peer={requester}");
            ProxyDiscoveryResponse { peers: Vec::new() }
        }
        Err(err) => {
            warn!("invalid proxy discovery request peer={requester} error={err}");
            ProxyDiscoveryResponse { peers: Vec::new() }
        }
    }
}

fn tenant_cert_is_valid(cert_store: &CertStore, owner_pubkey: &str) -> bool {
    cert_store
        .read()
        .ok()
        .and_then(|store| store.get(owner_pubkey).cloned())
        .is_some_and(|cert| !cert.is_expired())
}

fn handle_registration_rr_event(
    swarm: &mut Swarm<WorkloadBehaviour>,
    event: request_response::Event<Vec<u8>, Vec<u8>>,
    routing_table: &RoutingTable,
    cert_store: &CertStore,
) {
    match event {
        request_response::Event::Message { peer, message, .. } => match message {
            request_response::Message::Request {
                request, channel, ..
            } => match SidecarRegistration::from_bytes(&request) {
                Ok(reg) => {
                    let (ok, message) = evaluate_sidecar_registration(&reg, &peer, cert_store);
                    let has_capacity = routing_table
                        .read()
                        .map(|table| {
                            table.contains_key(&reg.manifest_id)
                                || table.len() < MAX_REGISTERED_SIDECARS
                        })
                        .unwrap_or(false);
                    let ok = ok && has_capacity;
                    let message = if ok || has_capacity {
                        message
                    } else {
                        "proxy sidecar route capacity reached".to_string()
                    };
                    if ok {
                        let entry = SidecarRouteEntry {
                            sidecar_peer_id: reg.sidecar_peer_id.clone(),
                            routes: reg.routes.clone(),
                            registered_at: p2p::timestamp_millis(),
                        };
                        {
                            let mut table =
                                routing_table.write().expect("routing table write lock");
                            table.insert(reg.manifest_id.clone(), entry);
                        }
                        info!(
                            "sidecar registered routes manifest={} peer={} routes={}",
                            reg.manifest_id,
                            reg.sidecar_peer_id,
                            reg.routes.len()
                        );
                    } else {
                        warn!(
                            "sidecar registration rejected manifest={} peer={} reason={}",
                            reg.manifest_id, peer, message
                        );
                    }
                    let ack = SidecarRegistrationAck {
                        manifest_id: reg.manifest_id.clone(),
                        ok,
                        message,
                    };
                    if let Err(err) = swarm
                        .behaviour_mut()
                        .registration_rr
                        .send_response(channel, ack.to_bytes())
                    {
                        warn!("failed to send registration ack error={:?}", err);
                    }
                }
                Err(err) => {
                    warn!(
                        "failed to deserialize sidecar registration from peer={} error={}",
                        peer, err
                    );
                }
            },
            request_response::Message::Response { .. } => {
                warn!("unexpected outbound response on registration protocol");
            }
        },
        request_response::Event::OutboundFailure { peer, error, .. } => {
            warn!(
                "registration outbound failure peer={} error={:?}",
                peer, error
            );
        }
        request_response::Event::InboundFailure { peer, error, .. } => {
            warn!(
                "registration inbound failure peer={} error={:?}",
                peer, error
            );
        }
        request_response::Event::ResponseSent { .. } => {}
    }
}

/// Evaluate a sidecar registration against the proxy's stored tenant certs and the
/// transport peer_id. Returns `(accepted, message)`.
///
/// Performs the four checks required by the sidecar-proxy-auth spec:
///   1. `sig` is a valid Ed25519 signature over `manifest_id || sidecar_peer_id` using
///      `sidecar_signing_pubkey`.
///   2. `owner_pubkey` matches at least one stored NodeCert's `owner_pubkey` (same tenant).
///   3. The transport peer_id of the connection equals `sidecar_peer_id`.
///   4. The matched cert is not expired.
pub fn evaluate_sidecar_registration(
    reg: &SidecarRegistration,
    transport_peer: &PeerId,
    cert_store: &CertStore,
) -> (bool, String) {
    if reg.sidecar_signing_pubkey.is_empty() {
        return (false, "missing sidecar_signing_pubkey".to_string());
    }

    let signed_data = format!("{}{}", reg.manifest_id, reg.sidecar_peer_id);
    if !verify_registration_sig(
        &reg.sidecar_signing_pubkey,
        &reg.sig,
        signed_data.as_bytes(),
    ) {
        return (false, "signature verification failed".to_string());
    }

    if transport_peer.to_string() != reg.sidecar_peer_id {
        return (
            false,
            format!(
                "transport peer_id {} does not match registration sidecar_peer_id {}",
                transport_peer, reg.sidecar_peer_id
            ),
        );
    }

    let store = match cert_store.read() {
        Ok(g) => g,
        Err(_) => {
            return (false, "cert store lock poisoned".to_string());
        }
    };

    if store.is_empty() {
        return (
            false,
            "proxy holds no tenant NodeCerts (run `podctl grant-proxy`)".to_string(),
        );
    }

    let cert = match store.get(&reg.owner_pubkey) {
        Some(c) => c,
        None => {
            return (
                false,
                format!(
                    "no NodeCert held for owner_pubkey {} — cross-tenant registration rejected",
                    reg.owner_pubkey
                ),
            );
        }
    };

    if cert.is_expired() {
        return (false, "tenant NodeCert is expired".to_string());
    }

    (true, "ok".to_string())
}

fn verify_registration_sig(signing_pubkey_b64: &str, sig_b64: &str, data: &[u8]) -> bool {
    let pk_bytes = match crypto::b64_decode(signing_pubkey_b64) {
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
    record: &DispatchRecord,
    pending_proxy_requests: &mut HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<anyhow::Result<ProxyHttpResponse>>,
    >,
) -> Result<(), Box<(ProxyPendingRequest, anyhow::Error)>> {
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
        return Err(Box::new((
            pending,
            anyhow!(format!(
                "no matching route for host {} path {}",
                host_msg, request_path
            )),
        )));
    }

    let peer_id = match record.peer_id.parse::<PeerId>() {
        Ok(peer) => peer,
        Err(err) => return Err(Box::new((pending, anyhow!("invalid peer id: {err}")))),
    };

    let request_id = swarm
        .behaviour_mut()
        .proxy_rr
        .send_request(&peer_id, pending.request);
    pending_proxy_requests.insert(request_id, pending.respond_to);
    Ok(())
}

fn select_route_port(record: &DispatchRecord, path: &str, host: Option<&str>) -> Option<u16> {
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

    if let Some(ref host_value) = normalized_host
        && candidates.peek().is_some()
        && let Some(route) = candidates
            .clone()
            .filter(|route| route.host.eq_ignore_ascii_case(host_value))
            .max_by_key(|route| route.path_prefix.len())
    {
        return Some(route.target_port);
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
async fn handle_egress_stream(peer: PeerId, stream: libp2p::Stream) -> anyhow::Result<()> {
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
        let response =
            EgressTunnelResponse::err(format!("unsupported protocol: {}", request.protocol));
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
    )
    .await
    {
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
    use crate::restapi::new_cert_store;
    use protocol::{NodeCert, NodeRole};

    #[test]
    fn test_proxy_peer_multiaddr_preserves_identity() {
        let peer_id = PeerId::random();
        let address: Multiaddr = format!("/ip4/192.0.2.1/udp/4002/quic-v1/p2p/{peer_id}")
            .parse()
            .unwrap();
        let peer = proxy_peer_from_multiaddr(&address).unwrap();
        assert_eq!(peer.peer_id, peer_id.to_string());
        assert_eq!(peer.addresses, vec![address.to_string()]);
    }

    #[test]
    fn test_observed_proxy_address_is_bound_to_connected_peer() {
        let peer_id = PeerId::random();
        let address: Multiaddr = "/ip4/192.0.2.2/udp/4002/quic-v1".parse().unwrap();
        let peer = proxy_peer_from_observed_addr(peer_id, address).unwrap();
        assert_eq!(peer.peer_id, peer_id.to_string());
        assert!(peer.addresses[0].ends_with(&format!("/p2p/{peer_id}")));
    }

    #[test]
    fn discovery_response_is_bounded_and_tenant_authorized() {
        let owner = "tenant-a";
        let requester = PeerId::random();
        let mut known = HashMap::new();
        for index in 0..3 {
            let peer_id = PeerId::random();
            known.insert(
                peer_id,
                ProxyPeer {
                    peer_id: peer_id.to_string(),
                    addresses: vec![format!(
                        "/ip4/192.0.2.{}/udp/4002/quic-v1/p2p/{peer_id}",
                        index + 1
                    )],
                },
            );
        }
        let store = new_cert_store();
        store.write().unwrap().insert(
            owner.to_string(),
            NodeCert {
                peer_id: PeerId::random().to_string(),
                kem_pubkey: String::new(),
                signing_pubkey: String::new(),
                capabilities: Vec::new(),
                role: NodeRole::Proxy,
                valid_until: u64::MAX,
                owner_pubkey: owner.to_string(),
                owner_sig: String::new(),
                endorsements: Vec::new(),
            },
        );
        let request = ProxyDiscoveryRequest {
            owner_pubkey: owner.to_string(),
            limit: 2,
        }
        .to_bytes()
        .unwrap();
        assert_eq!(
            build_discovery_response(&request, requester, &known, &store)
                .peers
                .len(),
            2
        );
        assert!(
            build_discovery_response(b"invalid", requester, &known, &store)
                .peers
                .is_empty()
        );
        let unauthorized = ProxyDiscoveryRequest {
            owner_pubkey: "tenant-b".to_string(),
            limit: 2,
        }
        .to_bytes()
        .unwrap();
        assert!(
            build_discovery_response(&unauthorized, requester, &known, &store)
                .peers
                .is_empty()
        );
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

    /// Build a tenant-issued NodeCert + sidecar signing keys, then a valid
    /// SidecarRegistration suitable for verification. Returns
    /// `(reg, transport_peer, cert_store_with_owner_cert_inserted)`.
    fn make_test_registration(
        sidecar_peer: PeerId,
        owner_b64: &str,
        owner_sk: &[u8],
        owner_pk: &[u8],
        sidecar_sk: &[u8],
        sidecar_pk: &[u8],
    ) -> (SidecarRegistration, PeerId, CertStore) {
        let manifest_id = "demo-manifest".to_string();
        let signed = format!("{}{}", manifest_id, sidecar_peer);
        let sig = crypto::sign_data_with_key(sidecar_sk, signed.as_bytes()).unwrap();

        let reg = SidecarRegistration {
            manifest_id,
            routes: vec![],
            sidecar_peer_id: sidecar_peer.to_string(),
            owner_pubkey: owner_b64.to_string(),
            sig: crypto::b64_encode(&sig),
            sidecar_signing_pubkey: crypto::b64_encode(sidecar_pk),
        };

        // Build proxy NodeCert signed by owner key (irrelevant fields set to dummies)
        let cert = NodeCert {
            peer_id: "QmProxy".to_string(),
            kem_pubkey: crypto::b64_encode(&[0u8; 32]),
            signing_pubkey: crypto::b64_encode(&[0u8; 32]),
            capabilities: vec!["proxy".to_string()],
            role: NodeRole::Proxy,
            valid_until: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 3600,
            owner_pubkey: owner_b64.to_string(),
            owner_sig: String::new(),
            endorsements: vec![],
        }
        .sign(owner_sk, owner_pk)
        .unwrap();

        let store = new_cert_store();
        store.write().unwrap().insert(owner_b64.to_string(), cert);
        (reg, sidecar_peer, store)
    }

    #[test]
    fn registration_accepted_when_owner_matches_stored_cert() {
        let (owner_pk, owner_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let (side_pk, side_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let owner_b64 = crypto::b64_encode(&owner_pk);
        let sidecar_peer = PeerId::random();
        let (reg, peer, store) = make_test_registration(
            sidecar_peer,
            &owner_b64,
            &owner_sk,
            &owner_pk,
            &side_sk,
            &side_pk,
        );
        let (ok, msg) = evaluate_sidecar_registration(&reg, &peer, &store);
        assert!(ok, "expected registration accepted: {}", msg);
    }

    #[test]
    fn registration_rejected_when_signature_invalid() {
        let (owner_pk, owner_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let (side_pk, side_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let owner_b64 = crypto::b64_encode(&owner_pk);
        let sidecar_peer = PeerId::random();
        let (mut reg, peer, store) = make_test_registration(
            sidecar_peer,
            &owner_b64,
            &owner_sk,
            &owner_pk,
            &side_sk,
            &side_pk,
        );
        // Tamper with the manifest_id so signed data no longer matches
        reg.manifest_id = "tampered".to_string();
        let (ok, _msg) = evaluate_sidecar_registration(&reg, &peer, &store);
        assert!(!ok);
    }

    #[test]
    fn registration_rejected_when_transport_peer_id_differs() {
        let (owner_pk, owner_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let (side_pk, side_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let owner_b64 = crypto::b64_encode(&owner_pk);
        let sidecar_peer = PeerId::random();
        let (reg, _peer, store) = make_test_registration(
            sidecar_peer,
            &owner_b64,
            &owner_sk,
            &owner_pk,
            &side_sk,
            &side_pk,
        );
        // Use a different peer id as the transport peer
        let other = PeerId::random();
        let (ok, _msg) = evaluate_sidecar_registration(&reg, &other, &store);
        assert!(!ok);
    }

    #[test]
    fn registration_rejected_when_owner_not_in_store() {
        let (owner_pk, owner_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let (side_pk, side_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let owner_b64 = crypto::b64_encode(&owner_pk);
        let sidecar_peer = PeerId::random();
        let (mut reg, peer, store) = make_test_registration(
            sidecar_peer,
            &owner_b64,
            &owner_sk,
            &owner_pk,
            &side_sk,
            &side_pk,
        );
        // Switch the owner_pubkey to a different one that is not stored.
        // Re-sign with the same sidecar key (so signature still valid) but using a fresh manifest
        // to keep signed data intact: manifest_id || sidecar_peer_id is unchanged. We only swap owner_pubkey.
        let (other_pk, _) = crypto::ensure_keypair_ephemeral().unwrap();
        reg.owner_pubkey = crypto::b64_encode(&other_pk);
        let (ok, msg) = evaluate_sidecar_registration(&reg, &peer, &store);
        assert!(!ok, "expected reject: {}", msg);
    }

    #[test]
    fn registration_rejected_when_no_certs_held() {
        let (owner_pk, _owner_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let (side_pk, side_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let owner_b64 = crypto::b64_encode(&owner_pk);
        let sidecar_peer = PeerId::random();
        let manifest_id = "demo".to_string();
        let signed = format!("{}{}", manifest_id, sidecar_peer);
        let sig = crypto::sign_data_with_key(&side_sk, signed.as_bytes()).unwrap();
        let reg = SidecarRegistration {
            manifest_id,
            routes: vec![],
            sidecar_peer_id: sidecar_peer.to_string(),
            owner_pubkey: owner_b64,
            sig: crypto::b64_encode(&sig),
            sidecar_signing_pubkey: crypto::b64_encode(&side_pk),
        };
        let store = new_cert_store();
        let (ok, _msg) = evaluate_sidecar_registration(&reg, &sidecar_peer, &store);
        assert!(!ok);
    }

    #[test]
    fn registration_rejected_when_cert_expired() {
        let (owner_pk, owner_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let (side_pk, side_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let owner_b64 = crypto::b64_encode(&owner_pk);
        let sidecar_peer = PeerId::random();

        let manifest_id = "demo".to_string();
        let signed = format!("{}{}", manifest_id, sidecar_peer);
        let sig = crypto::sign_data_with_key(&side_sk, signed.as_bytes()).unwrap();
        let reg = SidecarRegistration {
            manifest_id,
            routes: vec![],
            sidecar_peer_id: sidecar_peer.to_string(),
            owner_pubkey: owner_b64.clone(),
            sig: crypto::b64_encode(&sig),
            sidecar_signing_pubkey: crypto::b64_encode(&side_pk),
        };

        let cert = NodeCert {
            peer_id: "QmProxy".to_string(),
            kem_pubkey: crypto::b64_encode(&[0u8; 32]),
            signing_pubkey: crypto::b64_encode(&[0u8; 32]),
            capabilities: vec!["proxy".to_string()],
            role: NodeRole::Proxy,
            valid_until: 1, // expired
            owner_pubkey: owner_b64.clone(),
            owner_sig: String::new(),
            endorsements: vec![],
        }
        .sign(&owner_sk, &owner_pk)
        .unwrap();
        let store = new_cert_store();
        store.write().unwrap().insert(owner_b64, cert);
        let (ok, _msg) = evaluate_sidecar_registration(&reg, &sidecar_peer, &store);
        assert!(!ok);
    }
}
