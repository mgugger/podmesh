use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use log::{debug, info, warn};
use p2p::build_quic_multiaddr;
use p2p::http_proxy::{ProxyCodec, ProxyHttpRequest, ProxyHttpResponse};
use p2p::libp2p_stream;
use p2p::{
    handshake::{self, HandshakeDriveConfig, HandshakeState},
    request_response::HandshakeCodec,
};
use protocol::libp2p_constants::{
    EGRESS_TUNNEL_PROTOCOL, INGRESS_PROXY_PROTOCOL, PROXY_DISCOVERY_PROTOCOL,
    SIDECAR_REGISTRATION_PROTOCOL,
};
use protocol::machine::SidecarRouteSpec;
use protocol::{
    ProxyDiscoveryRequest, ProxyDiscoveryResponse, ProxyPeer, SidecarRegistration,
    SidecarRegistrationAck, SidecarRoute, validate_proxy_peers,
};
use reqwest::{
    Client, Method,
    header::{HeaderName, HeaderValue},
};
use tokio::signal;
use tokio::sync::{mpsc, oneshot};

pub mod egress_nft;
pub mod egress_proxy;
pub mod http_connect_proxy;
pub mod manifest_routes;

pub use ::p2p::identity::IdentitySource;
pub use http_connect_proxy::HTTP_CONNECT_PROXY_PORT;

pub const DEFAULT_SIDECAR_APP_PORT: u16 = 18080;
const REGISTRATION_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct SidecarConfig {
    pub identity: IdentitySource,
    pub proxy_peers: Vec<ProxyPeer>,
    pub lookup_interval: Duration,
    pub libp2p_host: String,
    pub libp2p_port: u16,
    pub manifest_id: String,
    pub ingress_host: String,
    pub app_port: u16,
    pub routes: Vec<SidecarRouteSpec>,
    pub owner_public_key_b64: Option<String>,
    /// Enable transparent egress proxy (requires CAP_NET_ADMIN)
    pub enable_egress: bool,
    /// Skip nftables programming even when egress is enabled (useful for tests or restricted hosts)
    pub skip_egress_nft: bool,
    /// Port for HTTP CONNECT proxy (explicit proxy mode)
    /// If set to 0, uses the default port. If None, HTTP CONNECT proxy is disabled.
    pub http_proxy_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarEvent {
    Connected {
        peer_id: String,
    },
    /// Proxy peer discovered for egress tunneling
    ProxyPeerDiscovered {
        peer_id: String,
    },
    /// Egress tunnel established to destination
    EgressTunnelEstablished {
        dest_host: String,
        dest_port: u16,
    },
    /// Egress tunnel failed
    EgressTunnelFailed {
        dest_host: String,
        dest_port: u16,
        error: String,
    },
}

pub async fn run_sidecar(cfg: SidecarConfig) -> Result<()> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        // Wait for either SIGTERM or SIGINT to gracefully shutdown
        tokio::select! {
            result = signal::ctrl_c() => {
                match result {
                    Ok(_) => info!("sidecar received SIGINT"),
                    Err(err) => warn!("sidecar ctrl+c listener failed error={}", err),
                }
            }
            _ = async {
                #[cfg(unix)]
                {
                    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
                        .expect("failed to install SIGTERM handler");
                    sigterm.recv().await;
                    info!("sidecar received SIGTERM");
                }
                #[cfg(not(unix))]
                {
                    std::future::pending::<()>().await;
                }
            } => {}
        }
        let _ = shutdown_tx.send(());
    });
    run_sidecar_with_shutdown(cfg, shutdown_rx, None).await
}

pub async fn run_sidecar_with_shutdown(
    cfg: SidecarConfig,
    mut shutdown: oneshot::Receiver<()>,
    event_tx: Option<mpsc::UnboundedSender<SidecarEvent>>,
) -> Result<()> {
    let listen_addr = cfg.listen_addr();
    let listen_addr_display = listen_addr
        .as_ref()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "none".to_string());
    info!(
        "sidecar runtime starting with config has_events={} manifest={} ingress_host={} libp2p_host={} libp2p_port={} lookup_interval_ms={} proxy_peers={} listen_addr={} app_port={} routes={} enable_egress={} skip_egress_nft={}",
        event_tx.is_some(),
        cfg.manifest_id,
        cfg.ingress_host,
        cfg.libp2p_host,
        cfg.libp2p_port,
        cfg.lookup_interval.as_millis() as u64,
        cfg.proxy_peers.len(),
        listen_addr_display,
        cfg.app_port,
        cfg.routes.len(),
        cfg.enable_egress,
        cfg.skip_egress_nft
    );

    let mut swarm = build_swarm(&cfg)?;
    if let Some(addr) = listen_addr {
        swarm
            .listen_on(addr)
            .context("start sidecar libp2p listener")?;
    }

    // Set up egress proxy if enabled
    let egress_nft_cleanup_needed = if cfg.enable_egress && !cfg.skip_egress_nft {
        if egress_nft::has_net_admin_capability() {
            match egress_nft::setup_egress_rules(&egress_nft::EgressNftConfig::default()) {
                Ok(()) => {
                    info!("egress nftables rules configured successfully");
                    true
                }
                Err(err) => {
                    warn!(
                        "failed to setup egress nftables rules (requires CAP_NET_ADMIN): {}",
                        err
                    );
                    false
                }
            }
        } else {
            warn!("skipping egress nftables setup: CAP_NET_ADMIN capability not available");
            false
        }
    } else {
        false
    };

    // Create channel for tunnel requests from egress proxy
    let (tunnel_tx, mut tunnel_rx) = mpsc::channel::<egress_proxy::TunnelRequest>(256);

    // Start transparent egress proxy listener if enabled
    let _egress_proxy_handle = if cfg.enable_egress {
        let egress_config = egress_proxy::EgressProxyConfig::default();
        let proxy = egress_proxy::EgressProxy::new(egress_config, tunnel_tx.clone());
        Some(tokio::spawn(async move {
            if let Err(err) = proxy.run().await {
                log::error!("egress proxy failed: {}", err);
            }
        }))
    } else {
        None
    };

    // Start HTTP CONNECT proxy if configured
    let _http_proxy_handle = if let Some(port) = cfg.http_proxy_port {
        let http_config = http_connect_proxy::HttpConnectProxyConfig {
            listen_port: if port == 0 {
                http_connect_proxy::HTTP_CONNECT_PROXY_PORT
            } else {
                port
            },
            listen_host: "127.0.0.1".to_string(),
        };
        let proxy = http_connect_proxy::HttpConnectProxy::new(http_config, tunnel_tx.clone());
        Some(tokio::spawn(async move {
            if let Err(err) = proxy.run().await {
                log::error!("HTTP CONNECT proxy failed: {}", err);
            }
        }))
    } else {
        None
    };

    // Get stream control for egress tunneling
    let egress_control = swarm.behaviour().egress_stream.new_control();
    let egress_protocol = StreamProtocol::try_from_owned(EGRESS_TUNNEL_PROTOCOL.to_string())
        .expect("valid egress protocol");

    let http_client = Client::builder()
        .build()
        .context("build sidecar http client")?;
    let mut state = SidecarState::new(http_client, &cfg)?;
    dial_proxy_peers(&mut swarm, &cfg);
    let (proxy_resp_tx, mut proxy_resp_rx) = mpsc::unbounded_channel();
    let mut handshake_ticker = tokio::time::interval(Duration::from_secs(1));
    handshake_ticker.tick().await;
    let mut discovery_ticker = tokio::time::interval(cfg.lookup_interval);
    discovery_ticker.tick().await;
    let mut registration_ticker = tokio::time::interval(REGISTRATION_REFRESH_INTERVAL);
    registration_ticker.tick().await;
    loop {
        tokio::select! {
            event = swarm.select_next_some() => {
                handle_swarm_event(&mut swarm, event, &cfg, &mut state, event_tx.as_ref(), &proxy_resp_tx)
            },
            _ = discovery_ticker.tick() => request_more_proxies(&mut swarm, &cfg, &mut state),
            _ = registration_ticker.tick() => refresh_proxy_registrations(&mut swarm, &cfg, &mut state),
            _ = &mut shutdown => {
                info!("sidecar shutdown requested");
                break;
            }
            _ = handshake_ticker.tick() => {
                let local_peer = *swarm.local_peer_id();
                match handshake::collect_handshake_actions(
                    &mut state.handshake_states,
                    &local_peer,
                    &HandshakeDriveConfig::default(),
                ) {
                    Ok(actions) => {
                        for (peer, payload) in actions.requests {
                            let request_id = swarm
                                .behaviour_mut()
                                .handshake_rr
                                .send_request(&peer, payload);
                            debug!("sidecar handshake request sent peer={} request_id={:?}", peer, request_id);
                        }

                        for peer in actions.drops {
                            debug!("sidecar removing peer after failed handshake attempts peer={}", peer);
                        }
                    }
                    Err(err) => {
                        warn!("sidecar handshake drive failed err={:?}", err);
                    }
                }
            },
            Some(pending) = proxy_resp_rx.recv() => {
                if let Err(err) = swarm.behaviour_mut().proxy_rr.send_response(pending.channel, pending.response) {
                    warn!("failed to send proxy response to workload error={:?}", err);
                }
            },
            // Handle egress tunnel requests
            Some(tunnel_req) = tunnel_rx.recv() => {
                if let Some(proxy_peer) = state.get_proxy_peer() {
                    let control = egress_control.clone();
                    let protocol = egress_protocol.clone();
                    tokio::spawn(async move {
                        handle_egress_tunnel(control, proxy_peer, protocol, tunnel_req).await;
                    });
                } else {
                    request_more_proxies(&mut swarm, &cfg, &mut state);
                    warn!("egress tunnel request dropped - no proxy peer discovered yet dest={}:{}",
                          tunnel_req.dest_host, tunnel_req.dest_port);
                }
            }
        }
    }

    // Clean up nftables rules on shutdown
    if egress_nft_cleanup_needed {
        if let Err(err) = egress_nft::cleanup_egress_rules() {
            warn!("failed to cleanup egress nftables rules: {}", err);
        } else {
            info!("egress nftables rules cleaned up");
        }
    }

    Ok(())
}

impl SidecarConfig {
    pub fn listen_addr(&self) -> Option<Multiaddr> {
        build_quic_multiaddr(&self.libp2p_host, self.libp2p_port)
    }
}

#[derive(NetworkBehaviour)]
struct SidecarBehaviour {
    handshake_rr: request_response::Behaviour<HandshakeCodec>,
    proxy_rr: request_response::Behaviour<ProxyCodec>,
    /// Stream behaviour for bidirectional egress tunneling
    egress_stream: libp2p_stream::Behaviour,
    /// Request-response for sidecar→proxy registration
    registration_rr: request_response::Behaviour<p2p::request_response::ByteCodec>,
    discovery_rr: request_response::Behaviour<p2p::request_response::ByteCodec>,
}

struct SidecarState {
    handshake_states: HashMap<PeerId, HandshakeState>,
    http_client: Client,
    proxy_peers: Vec<PeerId>,
    discovery_query_pending: bool,
    pending_registrations: HashMap<request_response::OutboundRequestId, PeerId>,
    /// Proxy peers whose tenant `NodeCert` we successfully verified during handshake.
    /// Only verified peers are eligible for `SidecarRegistration` and egress tunneling.
    verified_proxy_peers: HashSet<PeerId>,
}

impl SidecarState {
    fn new(http_client: Client, cfg: &SidecarConfig) -> Result<Self> {
        validate_proxy_peers(&cfg.proxy_peers, false)?;
        let proxy_peers = cfg
            .proxy_peers
            .iter()
            .map(|peer| {
                peer.peer_id
                    .parse::<PeerId>()
                    .context("parse configured proxy peer ID")
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            handshake_states: HashMap::new(),
            http_client,
            proxy_peers,
            discovery_query_pending: false,
            pending_registrations: HashMap::new(),
            verified_proxy_peers: HashSet::new(),
        })
    }

    /// Get a verified proxy peer for egress tunneling, if available.
    /// Spec requires that egress tunneling only goes through proxies whose
    /// tenant-signed NodeCert was verified during handshake.
    fn get_proxy_peer(&self) -> Option<PeerId> {
        self.proxy_peers
            .iter()
            .find(|p| self.verified_proxy_peers.contains(*p))
            .copied()
    }
}

struct PendingProxyResponse {
    channel: request_response::ResponseChannel<ProxyHttpResponse>,
    response: ProxyHttpResponse,
}

fn build_swarm(cfg: &SidecarConfig) -> Result<Swarm<SidecarBehaviour>> {
    let swarm = libp2p::SwarmBuilder::with_existing_identity(cfg.identity.load()?)
        .with_tokio()
        .with_quic()
        .with_dns()?
        .with_behaviour(|_key| {
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
            // Stream behaviour for bidirectional egress tunneling
            let egress_stream = libp2p_stream::Behaviour::new();
            let registration_rr = request_response::Behaviour::new(
                std::iter::once((
                    SIDECAR_REGISTRATION_PROTOCOL,
                    request_response::ProtocolSupport::Outbound,
                )),
                request_response::Config::default(),
            );
            let discovery_rr = request_response::Behaviour::new(
                std::iter::once((
                    PROXY_DISCOVERY_PROTOCOL,
                    request_response::ProtocolSupport::Outbound,
                )),
                request_response::Config::default(),
            );
            SidecarBehaviour {
                handshake_rr,
                proxy_rr,
                egress_stream,
                registration_rr,
                discovery_rr,
            }
        })?
        .build();

    debug!("local sidecar peer_id={}", swarm.local_peer_id());

    Ok(swarm)
}

fn dial_proxy_peers(swarm: &mut Swarm<SidecarBehaviour>, cfg: &SidecarConfig) {
    for peer in &cfg.proxy_peers {
        for address in &peer.addresses {
            dial_multiaddr_str(swarm, address);
        }
    }
}

fn handle_swarm_event(
    swarm: &mut Swarm<SidecarBehaviour>,
    event: SwarmEvent<SidecarBehaviourEvent>,
    cfg: &SidecarConfig,
    state: &mut SidecarState,
    event_tx: Option<&mpsc::UnboundedSender<SidecarEvent>>,
    proxy_resp_tx: &mpsc::UnboundedSender<PendingProxyResponse>,
) {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => {
            info!("sidecar listening for libp2p peers address={}", address);
        }
        SwarmEvent::ConnectionEstablished { peer_id, .. } => {
            debug!("sidecar connection established peer_id={}", peer_id);
            handshake::track_peer(&mut state.handshake_states, &peer_id);
            if state.proxy_peers.contains(&peer_id) {
                send_handshake_request_to_proxy(
                    swarm,
                    &peer_id,
                    cfg.owner_public_key_b64.as_deref(),
                );
                notify(
                    event_tx,
                    SidecarEvent::ProxyPeerDiscovered {
                        peer_id: peer_id.to_string(),
                    },
                );
            }

            notify(
                event_tx,
                SidecarEvent::Connected {
                    peer_id: peer_id.to_string(),
                },
            );
        }
        SwarmEvent::ConnectionClosed { peer_id, .. } => {
            debug!("sidecar connection closed peer_id={}", peer_id);
            handshake::untrack_peer(&mut state.handshake_states, &peer_id);
        }
        SwarmEvent::Behaviour(SidecarBehaviourEvent::HandshakeRr(event)) => {
            handle_handshake_event(swarm, event, cfg, state);
        }
        SwarmEvent::Behaviour(SidecarBehaviourEvent::ProxyRr(event)) => {
            handle_proxy_event(cfg, state, event, proxy_resp_tx);
        }
        SwarmEvent::Behaviour(SidecarBehaviourEvent::RegistrationRr(event)) => {
            handle_registration_rr_event(event, state);
        }
        SwarmEvent::Behaviour(SidecarBehaviourEvent::DiscoveryRr(event)) => {
            handle_discovery_event(swarm, event, cfg, state, event_tx);
        }
        _ => {}
    }
}

fn handle_handshake_event(
    swarm: &mut Swarm<SidecarBehaviour>,
    event: request_response::Event<Vec<u8>, Vec<u8>>,
    cfg: &SidecarConfig,
    state: &mut SidecarState,
) {
    match event {
        request_response::Event::Message { message, peer, .. } => {
            // Intercept response messages to verify proxy NodeCert before
            // delegating to the standard handshake handler. We only act on
            // responses; requests we receive are not from proxies in this
            // direction.
            if let request_response::Message::Response { ref response, .. } = message {
                verify_proxy_cert_from_response(swarm, peer, response, cfg, state);
            }

            handshake::handle_request_response_message(
                message,
                peer,
                &mut state.handshake_states,
                |response, channel| {
                    let _ = swarm
                        .behaviour_mut()
                        .handshake_rr
                        .send_response(channel, response);
                },
            );
        }
        request_response::Event::OutboundFailure { peer, error, .. } => {
            warn!(
                "sidecar handshake outbound failure peer={} error={:?}",
                peer, error
            );
            if matches!(
                error,
                request_response::OutboundFailure::UnsupportedProtocols
            ) {
                handshake::track_peer(&mut state.handshake_states, &peer).confirmed = true;
                debug!(
                    "sidecar treating peer as handshake-confirmed due to unsupported protocol peer={}",
                    peer
                );
            }
        }
        request_response::Event::InboundFailure { peer, error, .. } => {
            warn!(
                "sidecar handshake inbound failure peer={} error={:?}",
                peer, error
            );
        }
        request_response::Event::ResponseSent { peer, .. } => {
            debug!("sidecar handshake response sent peer={}", peer);
        }
    }
}

/// Inspect a handshake response from `peer`. If it carries a `proxy_cert_b64`,
/// validate it against the sidecar's tenant key and, if successful, mark the
/// peer as a verified proxy. If the peer is also one of our discovered tenant
/// proxy candidates, trigger registration immediately.
///
/// The four checks performed (mirroring spec scenario "Sidecar verifies a valid proxy cert"):
///   1. `NodeCert::verify()` — owner_sig is valid Ed25519
///   2. `NodeCert.owner_pubkey == cfg.owner_public_key_b64` — same tenant
///   3. `NodeCert.is_expired()` is false
///   4. `NodeCert.peer_id == peer.to_string()` — cert binds to this peer
fn verify_proxy_cert_from_response(
    swarm: &mut Swarm<SidecarBehaviour>,
    peer: PeerId,
    response: &[u8],
    cfg: &SidecarConfig,
    state: &mut SidecarState,
) {
    let cert_b64 = match handshake::extract_proxy_cert_from_response(response, &peer) {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };

    let Some(ref tenant_owner) = cfg.owner_public_key_b64 else {
        log::debug!(
            "received proxy cert from peer={} but sidecar has no tenant owner_pubkey configured",
            peer
        );
        return;
    };

    let cert = match protocol::NodeCert::from_b64(&cert_b64) {
        Ok(c) => c,
        Err(err) => {
            log::warn!(
                "failed to decode proxy NodeCert from peer={}: {}",
                peer,
                err
            );
            return;
        }
    };

    if let Err(err) = cert.verify() {
        log::warn!(
            "proxy NodeCert signature invalid peer={} error={}",
            peer,
            err
        );
        return;
    }

    if &cert.owner_pubkey != tenant_owner {
        log::warn!(
            "proxy NodeCert owner_pubkey mismatch peer={} cert_owner={} tenant_owner={}",
            peer,
            cert.owner_pubkey,
            tenant_owner
        );
        return;
    }

    if cert.is_expired() {
        log::warn!("proxy NodeCert is expired peer={}", peer);
        return;
    }

    if cert.peer_id != peer.to_string() {
        log::warn!(
            "proxy NodeCert peer_id mismatch peer={} cert_peer_id={}",
            peer,
            cert.peer_id
        );
        return;
    }

    log::info!(
        "verified proxy NodeCert peer={} owner_pubkey={} valid_until={}",
        peer,
        cert.owner_pubkey,
        cert.valid_until
    );
    state.verified_proxy_peers.insert(peer);

    // If this verified peer is one of our discovered tenant proxy candidates,
    // proceed with registration now (handshake completion + cert verification
    // are jointly the gate).
    if state.proxy_peers.contains(&peer) {
        send_sidecar_registration(swarm, cfg, peer, state);
    }
}

fn refresh_proxy_registrations(
    swarm: &mut Swarm<SidecarBehaviour>,
    cfg: &SidecarConfig,
    state: &mut SidecarState,
) {
    let peers: Vec<PeerId> = state.verified_proxy_peers.iter().copied().collect();
    for peer in peers {
        send_sidecar_registration(swarm, cfg, peer, state);
    }
}

fn handle_proxy_event(
    cfg: &SidecarConfig,
    state: &SidecarState,
    event: request_response::Event<ProxyHttpRequest, ProxyHttpResponse>,
    proxy_resp_tx: &mpsc::UnboundedSender<PendingProxyResponse>,
) {
    match event {
        request_response::Event::Message {
            peer,
            message,
            connection_id: _,
        } => match message {
            request_response::Message::Request {
                mut request,
                channel,
                request_id: _,
            } => {
                if request.target_port == 0 {
                    request.target_port = cfg.app_port;
                }
                info!(
                    "sidecar received proxy request peer={} manifest={} method={} path={} target_port={}",
                    peer,
                    request.manifest_id,
                    request.method,
                    request.path_and_query,
                    request.target_port
                );
                spawn_local_http_request(
                    state.http_client.clone(),
                    request,
                    channel,
                    proxy_resp_tx.clone(),
                );
            }
            request_response::Message::Response { response, .. } => {
                debug!(
                    "sidecar received proxy response acknowledgement peer={} status={}",
                    peer, response.status_code
                );
            }
        },
        request_response::Event::OutboundFailure { peer, error, .. } => {
            warn!(
                "sidecar proxy outbound failure peer={} error={:?}",
                peer, error
            );
        }
        request_response::Event::InboundFailure { peer, error, .. } => {
            warn!(
                "sidecar proxy inbound failure peer={} error={:?}",
                peer, error
            );
        }
        request_response::Event::ResponseSent { peer, .. } => {
            debug!("sidecar proxy response sent peer={}", peer);
        }
    }
}

fn spawn_local_http_request(
    client: Client,
    request: ProxyHttpRequest,
    channel: request_response::ResponseChannel<ProxyHttpResponse>,
    proxy_resp_tx: mpsc::UnboundedSender<PendingProxyResponse>,
) {
    tokio::spawn(async move {
        let manifest_id = request.manifest_id.clone();
        let method = request.method.clone();
        let path = if request.path_and_query.is_empty() {
            "/".to_string()
        } else {
            request.path_and_query.clone()
        };
        let target_port = request.target_port;
        let response = match execute_local_http_request(client, request).await {
            Ok(resp) => {
                log::info!(
                    "sidecar forwarded request to application manifest={} method={} path={} target_port={} status={}",
                    manifest_id,
                    method,
                    path,
                    target_port,
                    resp.status_code
                );
                resp
            }
            Err(err) => {
                log::warn!(
                    "sidecar local http request failed manifest={} method={} path={} target_port={} error={:?}",
                    manifest_id,
                    method,
                    path,
                    target_port,
                    err
                );
                ProxyHttpResponse {
                    status_code: 502,
                    headers: vec![("x-podmesh-error".into(), err.to_string())],
                    body: Vec::new(),
                }
            }
        };

        let _ = proxy_resp_tx.send(PendingProxyResponse { channel, response });
    });
}

async fn execute_local_http_request(
    client: Client,
    request: ProxyHttpRequest,
) -> Result<ProxyHttpResponse> {
    let target_port = request.target_port;
    let method = Method::from_bytes(request.method.as_bytes()).unwrap_or(Method::GET);
    let mut path = if request.path_and_query.is_empty() {
        "/".to_string()
    } else {
        request.path_and_query.clone()
    };
    if !path.starts_with('/') {
        path = format!("/{}", path);
    }
    let url = format!("http://127.0.0.1:{}{}", target_port, path);

    let mut builder = client.request(method, &url);
    for (name, value) in request.headers {
        if let (Ok(header_name), Ok(header_value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            builder = builder.header(header_name, header_value);
        }
    }

    let response = builder.body(request.body).send().await?;
    let status_code = response.status().as_u16();
    let mut headers = Vec::new();
    for (name, value) in response.headers().iter() {
        headers.push((
            name.as_str().to_string(),
            value.to_str().unwrap_or_default().to_string(),
        ));
    }
    let body = response.bytes().await?.to_vec();

    Ok(ProxyHttpResponse {
        status_code,
        headers,
        body,
    })
}

fn request_more_proxies(
    swarm: &mut Swarm<SidecarBehaviour>,
    cfg: &SidecarConfig,
    state: &mut SidecarState,
) {
    if state.discovery_query_pending {
        return;
    }
    let Some(owner_pubkey) = cfg.owner_public_key_b64.as_ref() else {
        return;
    };
    let Some(proxy_peer) = state.get_proxy_peer() else {
        dial_proxy_peers(swarm, cfg);
        return;
    };
    let request = ProxyDiscoveryRequest {
        owner_pubkey: owner_pubkey.clone(),
        limit: protocol::proxy_discovery::MAX_PROXY_PEERS as u16,
    };
    match request.to_bytes() {
        Ok(bytes) => {
            swarm
                .behaviour_mut()
                .discovery_rr
                .send_request(&proxy_peer, bytes);
            state.discovery_query_pending = true;
        }
        Err(err) => log::warn!("failed to encode proxy discovery request: {}", err),
    }
}

fn handle_discovery_event(
    swarm: &mut Swarm<SidecarBehaviour>,
    event: request_response::Event<Vec<u8>, Vec<u8>>,
    cfg: &SidecarConfig,
    state: &mut SidecarState,
    event_tx: Option<&mpsc::UnboundedSender<SidecarEvent>>,
) {
    match event {
        request_response::Event::Message {
            message: request_response::Message::Response { response, .. },
            ..
        } => {
            state.discovery_query_pending = false;
            match ProxyDiscoveryResponse::from_bytes(&response) {
                Ok(response) => {
                    add_discovered_proxy_peers(swarm, cfg, response.peers, state, event_tx)
                }
                Err(err) => log::warn!("invalid proxy discovery response: {}", err),
            }
        }
        request_response::Event::OutboundFailure { peer, error, .. } => {
            state.discovery_query_pending = false;
            log::warn!("proxy discovery failed peer={} error={:?}", peer, error);
        }
        request_response::Event::InboundFailure { peer, error, .. } => {
            log::warn!(
                "proxy discovery inbound failure peer={} error={:?}",
                peer,
                error
            );
        }
        request_response::Event::Message {
            message: request_response::Message::Request { .. },
            ..
        } => log::warn!("unexpected inbound proxy discovery request"),
        request_response::Event::ResponseSent { .. } => {}
    }
}

fn add_discovered_proxy_peers(
    swarm: &mut Swarm<SidecarBehaviour>,
    cfg: &SidecarConfig,
    peers: Vec<ProxyPeer>,
    state: &mut SidecarState,
    event_tx: Option<&mpsc::UnboundedSender<SidecarEvent>>,
) {
    if let Err(err) = validate_proxy_peers(&peers, true) {
        log::warn!("rejected proxy discovery peers: {}", err);
        return;
    }
    for candidate in peers {
        let Ok(peer_id) = candidate.peer_id.parse::<PeerId>() else {
            continue;
        };
        if state.proxy_peers.contains(&peer_id) {
            continue;
        }
        state.proxy_peers.push(peer_id);
        notify(
            event_tx,
            SidecarEvent::ProxyPeerDiscovered {
                peer_id: peer_id.to_string(),
            },
        );
        for address in candidate.addresses {
            dial_multiaddr_str(swarm, &address);
        }
        send_handshake_request_to_proxy(swarm, &peer_id, cfg.owner_public_key_b64.as_deref());
        if state.verified_proxy_peers.contains(&peer_id) {
            send_sidecar_registration(swarm, cfg, peer_id, state);
        }
    }
}

/// Build and send a single signed handshake request to a peer, bypassing the
/// drive-throttling logic. Used to force a Response from a tenant proxy so we
/// can extract its `proxy_cert_b64`.
fn send_handshake_request_to_proxy(
    swarm: &mut Swarm<SidecarBehaviour>,
    peer: &PeerId,
    owner_pubkey: Option<&str>,
) {
    let local_peer = *swarm.local_peer_id();
    let request = match owner_pubkey {
        Some(owner) => handshake::build_proxy_handshake_request(&local_peer, owner),
        None => handshake::build_handshake_request_for_kem_fetch(&local_peer),
    };
    match request {
        Ok(payload) => {
            let request_id = swarm
                .behaviour_mut()
                .handshake_rr
                .send_request(peer, payload);
            log::info!(
                "sidecar sent direct handshake request to tenant proxy peer={} request_id={:?}",
                peer,
                request_id
            );
        }
        Err(err) => {
            log::warn!(
                "sidecar failed to build handshake request for tenant proxy peer={}: {}",
                peer,
                err
            );
        }
    }
}

/// Handles an egress tunnel request by opening a stream to the proxy peer
/// and piping data bidirectionally.
async fn handle_egress_tunnel(
    mut control: libp2p_stream::Control,
    proxy_peer: PeerId,
    protocol: StreamProtocol,
    tunnel_req: egress_proxy::TunnelRequest,
) {
    use futures::io::{AsyncReadExt, AsyncWriteExt};
    use protocol::egress::{EgressTunnelRequest, EgressTunnelResponse};

    log::info!(
        "handling egress tunnel request dest={}:{} protocol={:?}",
        tunnel_req.dest_host,
        tunnel_req.dest_port,
        tunnel_req.protocol
    );

    // Open stream to proxy peer
    let mut p2p_stream = match control.open_stream(proxy_peer, protocol).await {
        Ok(stream) => stream,
        Err(err) => {
            log::error!(
                "failed to open egress stream to proxy peer={} dest={}:{} error={:?}",
                proxy_peer,
                tunnel_req.dest_host,
                tunnel_req.dest_port,
                err
            );
            return;
        }
    };

    // Send tunnel request header (using postcard, same as proxy)
    let request = EgressTunnelRequest::tcp(&tunnel_req.dest_host, tunnel_req.dest_port);
    let request_bytes = match postcard::to_allocvec(&request) {
        Ok(bytes) => bytes,
        Err(err) => {
            log::error!("failed to serialize egress request: {}", err);
            return;
        }
    };

    // Write length-prefixed request (little-endian, same as proxy)
    let len_bytes = (request_bytes.len() as u32).to_le_bytes();
    if let Err(err) = p2p_stream.write_all(&len_bytes).await {
        log::error!("failed to write egress request length: {}", err);
        return;
    }
    if let Err(err) = p2p_stream.write_all(&request_bytes).await {
        log::error!("failed to write egress request: {}", err);
        return;
    }
    if let Err(err) = p2p_stream.flush().await {
        log::error!("failed to flush egress request: {}", err);
        return;
    }

    // Read response (little-endian length prefix)
    let mut len_buf = [0u8; 4];
    if let Err(err) = p2p_stream.read_exact(&mut len_buf).await {
        log::error!("failed to read egress response length: {}", err);
        return;
    }
    let resp_len = u32::from_le_bytes(len_buf) as usize;
    if resp_len > 1024 * 1024 {
        log::error!("egress response too large: {} bytes", resp_len);
        return;
    }

    let mut resp_buf = vec![0u8; resp_len];
    if let Err(err) = p2p_stream.read_exact(&mut resp_buf).await {
        log::error!("failed to read egress response: {}", err);
        return;
    }

    let response: EgressTunnelResponse = match postcard::from_bytes(&resp_buf) {
        Ok(resp) => resp,
        Err(err) => {
            log::error!("failed to deserialize egress response: {}", err);
            return;
        }
    };

    if !response.success {
        log::error!(
            "egress tunnel failed dest={}:{} error={:?}",
            tunnel_req.dest_host,
            tunnel_req.dest_port,
            response.error
        );
        return;
    }

    log::info!(
        "egress tunnel established dest={}:{}",
        tunnel_req.dest_host,
        tunnel_req.dest_port
    );

    // Now pipe data bidirectionally between client_stream and p2p_stream
    // Use tokio's bidirectional copy which works with the Stream type
    let mut client_stream = tunnel_req.client_stream;

    // Send HTTP 200 response if this is an HTTP CONNECT proxy request
    if tunnel_req.send_http_200 {
        use tokio::io::AsyncWriteExt;
        let response = "HTTP/1.1 200 Connection Established\r\n\r\n";
        if let Err(err) = client_stream.write_all(response.as_bytes()).await {
            log::error!("failed to send HTTP 200 response: {}", err);
            return;
        }
    }

    // Convert libp2p Stream to tokio-compatible using compat layer
    let p2p_compat = tokio_util::compat::FuturesAsyncReadCompatExt::compat(p2p_stream);
    let (mut p2p_read, mut p2p_write) = tokio::io::split(p2p_compat);

    // If there's initial data (for plain HTTP proxy), send it to the destination first
    if let Some(initial_data) = tunnel_req.initial_data {
        use tokio::io::AsyncWriteExt;
        if let Err(err) = p2p_write.write_all(&initial_data).await {
            log::error!("failed to send initial data through tunnel: {}", err);
            return;
        }
    }

    let (mut client_read, mut client_write) = client_stream.into_split();

    // Wait for either direction to complete (or error)
    tokio::select! {
        result = tokio::io::copy(&mut client_read, &mut p2p_write) => {
            match result {
                Ok(bytes) => log::debug!("egress client->proxy completed bytes={}", bytes),
                Err(err) => log::debug!("egress client->proxy error: {}", err),
            }
        }
        result = tokio::io::copy(&mut p2p_read, &mut client_write) => {
            match result {
                Ok(bytes) => log::debug!("egress proxy->client completed bytes={}", bytes),
                Err(err) => log::debug!("egress proxy->client error: {}", err),
            }
        }
    }

    log::debug!(
        "egress tunnel closed dest={}:{}",
        tunnel_req.dest_host,
        tunnel_req.dest_port
    );
}

fn dial_multiaddr_str(swarm: &mut Swarm<SidecarBehaviour>, addr: &str) {
    match addr.parse::<Multiaddr>() {
        Ok(ma) => dial_multiaddr(swarm, &ma),
        Err(err) => log::warn!("invalid proxy multiaddr addr={} error={}", addr, err),
    }
}

fn dial_multiaddr(swarm: &mut Swarm<SidecarBehaviour>, addr: &Multiaddr) {
    if let Err(err) = swarm.dial(addr.clone()) {
        log::warn!(
            "sidecar failed to dial proxy peer addr={} error={}",
            addr,
            err
        );
    } else {
        log::debug!("sidecar dialing proxy peer addr={}", addr);
    }
}

fn notify(event_tx: Option<&mpsc::UnboundedSender<SidecarEvent>>, event: SidecarEvent) {
    if let Some(tx) = event_tx
        && let Err(err) = tx.send(event)
    {
        log::warn!("sidecar failed to emit event error={}", err);
    }
}

/// Sign `manifest_id || sidecar_peer_id` with the node's Ed25519 signing key.
/// Returns `(sig_b64, sidecar_signing_pubkey_b64)`.
fn sign_registration_payload(
    manifest_id: &str,
    sidecar_peer_id: &str,
) -> anyhow::Result<(String, String)> {
    let data = format!("{}{}", manifest_id, sidecar_peer_id);
    let (pub_bytes, priv_bytes) = crypto::ensure_keypair_on_disk()?;
    let sig_bytes = crypto::sign_data_with_key(&priv_bytes, data.as_bytes())?;
    Ok((
        crypto::b64_encode(&sig_bytes),
        crypto::b64_encode(&pub_bytes),
    ))
}

/// Send a SidecarRegistration to the given proxy peer.
fn send_sidecar_registration(
    swarm: &mut Swarm<SidecarBehaviour>,
    cfg: &SidecarConfig,
    proxy_peer: PeerId,
    state: &mut SidecarState,
) {
    let Some(ref owner_pub) = cfg.owner_public_key_b64 else {
        return;
    };

    // Spec gating: only register with proxies whose tenant-signed NodeCert we
    // have already verified during handshake. Without verification the
    // `owner_pubkey` exchange has no integrity guarantee.
    if !state.verified_proxy_peers.contains(&proxy_peer) {
        log::debug!(
            "skipping registration to unverified proxy peer={} (cert not yet validated)",
            proxy_peer
        );
        return;
    }
    if state
        .pending_registrations
        .values()
        .any(|pending_peer| *pending_peer == proxy_peer)
    {
        return;
    }

    let sidecar_peer_id = swarm.local_peer_id().to_string();
    let (sig, sidecar_signing_pubkey) =
        match sign_registration_payload(&cfg.manifest_id, &sidecar_peer_id) {
            Ok(s) => s,
            Err(err) => {
                log::warn!("sidecar failed to sign registration payload: {}", err);
                return;
            }
        };

    let routes: Vec<SidecarRoute> = cfg
        .routes
        .iter()
        .map(|r| SidecarRoute {
            path_prefix: r.path_prefix.clone(),
            port: r.target_port,
        })
        .collect();

    let reg = SidecarRegistration {
        manifest_id: cfg.manifest_id.clone(),
        routes,
        sidecar_peer_id: sidecar_peer_id.clone(),
        owner_pubkey: owner_pub.clone(),
        sig,
        sidecar_signing_pubkey,
    };

    log::info!(
        "sidecar sending registration to proxy peer={} manifest={} routes={}",
        proxy_peer,
        cfg.manifest_id,
        cfg.routes.len()
    );
    let request_id = swarm
        .behaviour_mut()
        .registration_rr
        .send_request(&proxy_peer, reg.to_bytes());
    state.pending_registrations.insert(request_id, proxy_peer);
}

/// Handle incoming registration ack from proxy.
fn handle_registration_rr_event(
    event: request_response::Event<Vec<u8>, Vec<u8>>,
    state: &mut SidecarState,
) {
    match event {
        request_response::Event::Message { peer, message, .. } => match message {
            request_response::Message::Response {
                request_id,
                response,
                ..
            } => {
                state.pending_registrations.remove(&request_id);
                match SidecarRegistrationAck::from_bytes(&response) {
                    Ok(ack) => {
                        if ack.ok {
                            log::info!(
                                "sidecar registration acknowledged by proxy peer={} manifest={}",
                                peer,
                                ack.manifest_id
                            );
                        } else {
                            log::warn!(
                                "sidecar registration rejected by proxy peer={} manifest={} reason={}",
                                peer,
                                ack.manifest_id,
                                ack.message
                            );
                        }
                    }
                    Err(err) => {
                        log::warn!(
                            "failed to deserialize registration ack from peer={}: {}",
                            peer,
                            err
                        );
                    }
                }
            }
            request_response::Message::Request { .. } => {
                log::warn!("unexpected inbound registration request on sidecar");
            }
        },
        request_response::Event::OutboundFailure {
            peer,
            request_id,
            error,
            ..
        } => {
            log::warn!(
                "sidecar registration outbound failure peer={} error={:?}",
                peer,
                error
            );
            state.pending_registrations.remove(&request_id);
        }
        request_response::Event::InboundFailure { peer, error, .. } => {
            log::warn!(
                "sidecar registration inbound failure peer={} error={:?}",
                peer,
                error
            );
        }
        request_response::Event::ResponseSent { .. } => {}
    }
}
