use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::{StreamExt, future};
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm,
    kad::{self, Quorum, Record, RecordKey},
    multiaddr::Protocol,
    request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use log::{debug, info, warn};
use p2p::http_proxy::{ProxyCodec, ProxyHttpRequest, ProxyHttpResponse};
use p2p::libp2p_stream;
use p2p::{
    build_quic_multiaddr, parse_bootstrap_peer, sidecar_manifest::sign_sidecar_manifest_record,
    timestamp_millis,
};
use p2p::{
    handshake::{self, HandshakeDriveConfig, HandshakeState},
    request_response::{HandshakeCodec, ManifestFetchCodec},
};
use protocol::libp2p_constants::{
    EGRESS_TUNNEL_PROTOCOL, INGRESS_PROXY_PROTOCOL, MANIFEST_RECORD_PREFIX, MANIFEST_RECORD_TTL_MS,
    PROXY_PROVIDER_KEY, SIDECAR_MANIFEST_PROTOCOL, SIDECAR_REGISTRATION_PROTOCOL,
};
use protocol::machine::{
    SidecarRouteSpec, build_sidecar_provider_record, root_as_sidecar_manifest_request,
};
use protocol::{SidecarRegistration, SidecarRegistrationAck, SidecarRoute};
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

pub use http_connect_proxy::HTTP_CONNECT_PROXY_PORT;

pub const DEFAULT_SIDECAR_APP_PORT: u16 = 18080;
const MANIFEST_RECORD_VERSION: u16 = 1;

#[derive(Clone, Debug)]
pub struct SidecarConfig {
    pub provider_label: String,
    pub bootstrap_peers: Vec<String>,
    pub bootstrap_peer_ip: Option<String>,
    pub lookup_interval: Duration,
    pub announce_interval: Duration,
    pub libp2p_host: String,
    pub libp2p_port: u16,
    pub announce_providers: bool,
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
    ProviderDiscovered {
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
        "sidecar runtime starting with config has_events={} provider={} manifest={} ingress_host={} libp2p_host={} libp2p_port={} announce_providers={} lookup_interval_ms={} announce_interval_ms={} bootstrap_peers={:?} bootstrap_peer_ip={} listen_addr={} app_port={} routes={} enable_egress={} skip_egress_nft={}",
        event_tx.is_some(),
        cfg.provider_label,
        cfg.manifest_id,
        cfg.ingress_host,
        cfg.libp2p_host,
        cfg.libp2p_port,
        cfg.announce_providers,
        cfg.lookup_interval.as_millis() as u64,
        cfg.announce_interval.as_millis() as u64,
        cfg.bootstrap_peers,
        cfg.bootstrap_peer_ip.as_deref().unwrap_or("none"),
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

    dial_bootstrap(&mut swarm, &cfg);

    let mut lookup_ticker = tokio::time::interval(cfg.lookup_interval);
    lookup_ticker.tick().await;
    trigger_lookup(&mut swarm, &cfg);

    let mut announce_ticker = if cfg.announce_providers {
        let mut ticker = tokio::time::interval(cfg.announce_interval);
        ticker.tick().await;
        announce_provider(&mut swarm, &cfg);
        Some(ticker)
    } else {
        None
    };

    let http_client = Client::builder()
        .build()
        .context("build sidecar http client")?;
    let mut state = SidecarState::new(http_client);
    let (proxy_resp_tx, mut proxy_resp_rx) = mpsc::unbounded_channel();
    let mut handshake_ticker = tokio::time::interval(Duration::from_secs(1));
    handshake_ticker.tick().await;
    let mut manifest_ticker = tokio::time::interval(cfg.announce_interval);
    manifest_ticker.tick().await;
    // Proxy lookup ticker (active if any egress mode is enabled)
    let needs_egress_proxy = cfg.enable_egress || cfg.http_proxy_port.is_some();
    let mut proxy_lookup_ticker = if needs_egress_proxy {
        let mut ticker = tokio::time::interval(Duration::from_secs(60)); // Refresh proxy peers every 60s
        ticker.tick().await; // Skip first immediate tick
        Some(ticker)
    } else {
        None
    };
    // Don't publish manifest immediately - wait for bootstrap to complete
    loop {
        tokio::select! {
            event = swarm.select_next_some() => {
                handle_swarm_event(&mut swarm, event, &cfg, &mut state, event_tx.as_ref(), &proxy_resp_tx)
            },
            _ = lookup_ticker.tick() => trigger_lookup(&mut swarm, &cfg),
            _ = async {
                if let Some(ticker) = announce_ticker.as_mut() {
                    ticker.tick().await;
                } else {
                    future::pending::<()>().await;
                }
            } => {
                if cfg.announce_providers && state.kad_bootstrap_complete {
                    announce_provider(&mut swarm, &cfg);
                }
            },
            // Periodic proxy provider refresh
            _ = async {
                if let Some(ticker) = proxy_lookup_ticker.as_mut() {
                    ticker.tick().await;
                } else {
                    future::pending::<()>().await;
                }
            } => {
                if needs_egress_proxy && state.kad_bootstrap_complete {
                    trigger_tenant_proxy_lookup(&mut swarm, &cfg, &mut state);
                }
            },
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
                            swarm.behaviour_mut().kademlia.remove_peer(&peer);
                        }
                    }
                    Err(err) => {
                        warn!("sidecar handshake drive failed err={:?}", err);
                    }
                }
            },
            _ = manifest_ticker.tick() => {
                if state.kad_bootstrap_complete {
                    publish_manifest_record(&mut swarm, &cfg);
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
                    // No proxy peer known yet, try to discover one
                    if needs_egress_proxy && !state.proxy_query_pending {
                        trigger_tenant_proxy_lookup(&mut swarm, &cfg, &mut state);
                    }
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
    pub fn record_key(&self) -> RecordKey {
        RecordKey::new(&self.provider_label)
    }

    pub fn manifest_record_key(&self) -> RecordKey {
        let key = format!("{}{}", MANIFEST_RECORD_PREFIX, self.manifest_id);
        RecordKey::new(&key)
    }

    pub fn listen_addr(&self) -> Option<Multiaddr> {
        build_quic_multiaddr(&self.libp2p_host, self.libp2p_port)
    }

    pub fn bootstrap_peer_multiaddr(&self) -> Option<Multiaddr> {
        let raw = self.bootstrap_peer_ip.as_deref()?;
        parse_bootstrap_peer(raw, self.libp2p_port)
    }
}

#[derive(NetworkBehaviour)]
struct SidecarBehaviour {
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    handshake_rr: request_response::Behaviour<HandshakeCodec>,
    proxy_rr: request_response::Behaviour<ProxyCodec>,
    manifest_rr: request_response::Behaviour<ManifestFetchCodec>,
    /// Stream behaviour for bidirectional egress tunneling
    egress_stream: libp2p_stream::Behaviour,
    /// Request-response for sidecar→proxy registration
    registration_rr: request_response::Behaviour<p2p::request_response::ByteCodec>,
}

struct SidecarState {
    known_providers: HashSet<String>,
    handshake_states: HashMap<PeerId, HandshakeState>,
    kad_bootstrapped: bool,
    kad_bootstrap_complete: bool,
    http_client: Client,
    /// Discovered proxy peers for egress tunneling (from DHT provider queries)
    proxy_peers: Vec<PeerId>,
    /// Whether we have an active proxy provider query
    proxy_query_pending: bool,
    /// Whether we have triggered the tenant proxy registration lookup
    tenant_proxy_lookup_triggered: bool,
    /// Pending outbound registration request id
    pending_registration_id: Option<request_response::OutboundRequestId>,
    /// Proxy peers whose tenant `NodeCert` we successfully verified during handshake.
    /// Only verified peers are eligible for `SidecarRegistration` and egress tunneling.
    verified_proxy_peers: HashSet<PeerId>,
}

impl SidecarState {
    fn new(http_client: Client) -> Self {
        Self {
            known_providers: HashSet::new(),
            handshake_states: HashMap::new(),
            kad_bootstrapped: false,
            kad_bootstrap_complete: false,
            http_client,
            proxy_peers: Vec::new(),
            proxy_query_pending: false,
            tenant_proxy_lookup_triggered: false,
            pending_registration_id: None,
            verified_proxy_peers: HashSet::new(),
        }
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

fn build_swarm(_cfg: &SidecarConfig) -> Result<Swarm<SidecarBehaviour>> {
    let swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_quic()
        .with_dns()?
        .with_behaviour(|key| {
            let peer_id = key.public().to_peer_id();
            let store = kad::store::MemoryStore::new(peer_id);
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
            // Stream behaviour for bidirectional egress tunneling
            let egress_stream = libp2p_stream::Behaviour::new();
            let registration_rr = request_response::Behaviour::new(
                std::iter::once((
                    SIDECAR_REGISTRATION_PROTOCOL,
                    request_response::ProtocolSupport::Outbound,
                )),
                request_response::Config::default(),
            );
            let mut behaviour = SidecarBehaviour {
                kademlia: kad::Behaviour::new(peer_id, store),
                handshake_rr,
                proxy_rr,
                manifest_rr,
                egress_stream,
                registration_rr,
            };
            behaviour.kademlia.set_mode(Some(kad::Mode::Client));
            behaviour
        })?
        .build();

    debug!("local sidecar peer_id={}", swarm.local_peer_id());
    let kad_protocols: Vec<String> = swarm
        .behaviour()
        .kademlia
        .protocol_names()
        .iter()
        .map(|p| p.to_string())
        .collect();
    debug!(
        "sidecar kad protocols configured kad_protocols={:?}",
        kad_protocols
    );

    Ok(swarm)
}

fn dial_bootstrap(swarm: &mut Swarm<SidecarBehaviour>, cfg: &SidecarConfig) {
    for addr in &cfg.bootstrap_peers {
        dial_multiaddr_str(swarm, addr);
    }

    if let Some(addr) = cfg.bootstrap_peer_multiaddr() {
        dial_multiaddr(swarm, &addr);
    } else if cfg.bootstrap_peer_ip.is_some() {
        warn!(
            "sidecar bootstrap ip provided but no valid multiaddr could be built ip={} port={}",
            cfg.bootstrap_peer_ip.as_deref().unwrap_or(""),
            cfg.libp2p_port
        );
    }
}

fn trigger_lookup(swarm: &mut Swarm<SidecarBehaviour>, cfg: &SidecarConfig) {
    let key = cfg.record_key();
    let query_id = swarm.behaviour_mut().kademlia.get_providers(key);
    debug!(
        "sidecar started provider lookup query_id={:?} provider={}",
        query_id, cfg.provider_label
    );
}

fn announce_provider(swarm: &mut Swarm<SidecarBehaviour>, cfg: &SidecarConfig) {
    let key = cfg.record_key();
    match swarm.behaviour_mut().kademlia.start_providing(key) {
        Ok(query_id) => {
            debug!(
                "sidecar announced provider query_id={:?} provider={}",
                query_id, cfg.provider_label
            )
        }
        Err(err) => {
            warn!(
                "sidecar provider announce failed provider={} error={}",
                cfg.provider_label, err
            )
        }
    }
}

fn publish_manifest_record(swarm: &mut Swarm<SidecarBehaviour>, cfg: &SidecarConfig) {
    let timestamp_ms = timestamp_millis();
    let payload = build_manifest_record_payload(swarm.local_peer_id(), cfg, timestamp_ms);
    let record_key = cfg.manifest_record_key();
    info!(
        "sidecar publishing ingress manifest to dht manifest={} ingress_host={} record_key={:?}",
        cfg.manifest_id, cfg.ingress_host, record_key
    );

    let record = Record {
        key: record_key.clone(),
        value: payload,
        publisher: Some(*swarm.local_peer_id()),
        expires: Some(Instant::now() + Duration::from_millis(MANIFEST_RECORD_TTL_MS as u64)),
    };

    match swarm
        .behaviour_mut()
        .kademlia
        .put_record(record, Quorum::One)
    {
        Ok(query_id) => {
            debug!(
                "sidecar published manifest record query_id={:?} manifest={}",
                query_id, cfg.manifest_id
            )
        }
        Err(err) => {
            warn!(
                "sidecar failed to publish manifest record manifest={} error={}",
                cfg.manifest_id, err
            )
        }
    }

    match swarm.behaviour_mut().kademlia.start_providing(record_key) {
        Ok(query_id) => {
            debug!(
                "sidecar announced manifest provider query_id={:?} manifest={}",
                query_id, cfg.manifest_id
            )
        }
        Err(err) => {
            warn!(
                "sidecar manifest provider announce failed manifest={} error={}",
                cfg.manifest_id, err
            )
        }
    }
}

fn build_manifest_record_payload(
    peer_id: &PeerId,
    cfg: &SidecarConfig,
    timestamp_ms: u64,
) -> Vec<u8> {
    let peer_id = peer_id.to_string();
    build_sidecar_provider_record(protocol::machine::SidecarProviderRecordParams {
        manifest_id: &cfg.manifest_id,
        peer_id: &peer_id,
        host: &cfg.ingress_host,
        owner_public_key_b64: cfg.owner_public_key_b64.as_deref(),
        routes: &cfg.routes,
        ttl_ms: MANIFEST_RECORD_TTL_MS,
        last_updated_ms: timestamp_ms,
        version: MANIFEST_RECORD_VERSION,
    })
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
        SwarmEvent::ConnectionEstablished {
            peer_id, endpoint, ..
        } => {
            debug!("sidecar connection established peer_id={}", peer_id);
            handshake::track_peer(&mut state.handshake_states, &peer_id);
            let addr = endpoint.get_remote_address().clone();
            swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);

            if !state.kad_bootstrapped {
                match swarm.behaviour_mut().kademlia.bootstrap() {
                    Ok(_) => {
                        state.kad_bootstrapped = true;
                        debug!("sidecar initiated kademlia bootstrap");
                    }
                    Err(err) => {
                        debug!("sidecar kademlia bootstrap attempt failed err={}", err);
                    }
                }
            }

            notify(
                event_tx,
                SidecarEvent::Connected {
                    peer_id: peer_id.to_string(),
                },
            );
        }
        SwarmEvent::ConnectionClosed {
            peer_id,
            num_established,
            ..
        } => {
            debug!("sidecar connection closed peer_id={}", peer_id);
            handshake::untrack_peer(&mut state.handshake_states, &peer_id);
            if num_established == 0 {
                swarm.behaviour_mut().kademlia.remove_peer(&peer_id);
            }
        }
        SwarmEvent::Behaviour(SidecarBehaviourEvent::Kademlia(event)) => {
            handle_kad_event(swarm, event, cfg, state, event_tx);
        }
        SwarmEvent::Behaviour(SidecarBehaviourEvent::HandshakeRr(event)) => {
            handle_handshake_event(swarm, event, cfg, state);
        }
        SwarmEvent::Behaviour(SidecarBehaviourEvent::ProxyRr(event)) => {
            handle_proxy_event(cfg, state, event, proxy_resp_tx);
        }
        SwarmEvent::Behaviour(SidecarBehaviourEvent::ManifestRr(event)) => {
            handle_manifest_fetch_event(swarm, cfg, event);
        }
        SwarmEvent::Behaviour(SidecarBehaviourEvent::RegistrationRr(event)) => {
            handle_registration_rr_event(event, state);
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
    let newly_verified = state.verified_proxy_peers.insert(peer);

    // If this verified peer is one of our discovered tenant proxy candidates,
    // proceed with registration now (handshake completion + cert verification
    // are jointly the gate).
    if newly_verified && state.proxy_peers.contains(&peer) {
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

fn handle_manifest_fetch_event(
    swarm: &mut Swarm<SidecarBehaviour>,
    cfg: &SidecarConfig,
    event: request_response::Event<Vec<u8>, Vec<u8>>,
) {
    match event {
        request_response::Event::Message { peer, message, .. } => match message {
            request_response::Message::Request {
                request, channel, ..
            } => {
                let response =
                    match build_manifest_response_bytes(swarm.local_peer_id(), cfg, &request) {
                        Ok(bytes) => bytes,
                        Err(err) => {
                            log::warn!(
                                "sidecar failed to build manifest response peer={} error={}",
                                peer,
                                err
                            );
                            Vec::new()
                        }
                    };
                if let Err(err) = swarm
                    .behaviour_mut()
                    .manifest_rr
                    .send_response(channel, response)
                {
                    log::warn!(
                        "sidecar failed to send manifest response peer={} error={:?}",
                        peer,
                        err
                    );
                }
            }
            request_response::Message::Response { .. } => {
                log::debug!(
                    "sidecar received unexpected manifest response peer={}",
                    peer
                );
            }
        },
        request_response::Event::OutboundFailure { peer, error, .. } => {
            log::warn!(
                "sidecar manifest outbound failure peer={} error={:?}",
                peer,
                error
            );
        }
        request_response::Event::InboundFailure { peer, error, .. } => {
            log::warn!(
                "sidecar manifest inbound failure peer={} error={:?}",
                peer,
                error
            );
        }
        request_response::Event::ResponseSent { peer, .. } => {
            log::debug!("sidecar manifest response sent peer={}", peer);
        }
    }
}

fn build_manifest_response_bytes(
    peer_id: &PeerId,
    cfg: &SidecarConfig,
    request_bytes: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let request =
        root_as_sidecar_manifest_request(request_bytes).context("parse manifest fetch request")?;
    if request.manifest_id != cfg.manifest_id {
        anyhow::bail!(
            "received manifest request for {} but serving {}",
            request.manifest_id,
            cfg.manifest_id
        );
    }

    let timestamp_ms = timestamp_millis();
    let payload = build_manifest_record_payload(peer_id, cfg, timestamp_ms);
    sign_sidecar_manifest_record(&payload)
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

fn handle_kad_event(
    swarm: &mut Swarm<SidecarBehaviour>,
    event: kad::Event,
    cfg: &SidecarConfig,
    state: &mut SidecarState,
    event_tx: Option<&mpsc::UnboundedSender<SidecarEvent>>,
) {
    let proxy_record_key = RecordKey::new(&PROXY_PROVIDER_KEY);
    // Compute the tenant-specific proxy DHT key if owner pubkey is available
    let tenant_proxy_key: Option<RecordKey> = cfg
        .owner_public_key_b64
        .as_deref()
        .and_then(|pk| compute_tenant_proxy_dht_key(pk).ok())
        .map(|k| RecordKey::new(&k));
    match event {
        kad::Event::OutboundQueryProgressed { result, .. } => match result {
            kad::QueryResult::GetProviders(Ok(ok)) => match ok {
                kad::GetProvidersOk::FoundProviders { key, providers } => {
                    log::debug!(
                        "sidecar get_providers result key={:?} count={}",
                        key,
                        providers.len()
                    );
                    // Spec: the global `podmesh-proxy-node` key MUST NOT be used by sidecars to
                    // discover authenticated proxies. We still allow this lookup result to
                    // populate generic provider awareness for backward compatibility with
                    // legacy mesh tooling, but we no longer use these peers for egress
                    // tunneling or registration. Tenant binding requires the obfuscated key.
                    if key == proxy_record_key {
                        update_provider_cache_for_proxy(providers.clone(), state, event_tx);
                    }
                    if key == cfg.record_key() {
                        // Provider discovery for our provider_label (may overlap with proxy key)
                        update_provider_cache(providers.clone(), state, event_tx);
                    }
                    // Phase 7.3: tenant proxy DHT lookup result. Mark these as candidate
                    // proxy peers (used for egress + registration after cert verification).
                    if let Some(ref tpk) = tenant_proxy_key
                        && &key == tpk
                    {
                        update_tenant_proxy_peers(swarm, cfg, providers, state, event_tx);
                    }
                }
                kad::GetProvidersOk::FinishedWithNoAdditionalRecord { closest_peers } => {
                    log::debug!(
                        "sidecar provider lookup finished without providers provider={} closest={}",
                        cfg.provider_label,
                        closest_peers.len()
                    );
                    // Mark proxy query as complete if this was a proxy query
                    state.proxy_query_pending = false;
                }
            },
            kad::QueryResult::GetProviders(Err(err)) => {
                log::warn!(
                    "sidecar provider lookup failed provider={} error={}",
                    cfg.provider_label,
                    err
                );
                state.proxy_query_pending = false;
            }
            kad::QueryResult::Bootstrap(Ok(_)) => {
                if !state.kad_bootstrap_complete {
                    state.kad_bootstrap_complete = true;
                    log::info!("sidecar kademlia bootstrap completed, publishing manifest");
                    publish_manifest_record(swarm, cfg);
                    if cfg.announce_providers {
                        announce_provider(swarm, cfg);
                    }
                    // Phase 7.3: trigger tenant proxy DHT lookup for sidecar registration AND egress.
                    // Note: we no longer trigger the legacy `podmesh-proxy-node` lookup for workload
                    // traffic per the sidecar-proxy-auth spec.
                    if !state.tenant_proxy_lookup_triggered {
                        trigger_tenant_proxy_lookup(swarm, cfg, state);
                    }
                }
            }
            kad::QueryResult::Bootstrap(Err(err)) => {
                log::warn!("sidecar kademlia bootstrap failed error={}", err);
            }
            _ => {}
        },
        kad::Event::RoutingUpdated { peer, .. } => {
            log::debug!("sidecar routing entry updated peer={}", peer);
        }
        _ => {}
    }
}

fn update_provider_cache<I>(
    providers: I,
    state: &mut SidecarState,
    event_tx: Option<&mpsc::UnboundedSender<SidecarEvent>>,
) where
    I: IntoIterator<Item = libp2p::PeerId>,
{
    let mut changed = false;
    for id in providers {
        let peer = id.to_string();
        if state.known_providers.insert(peer.clone()) {
            log::info!("sidecar discovered provider peer={}", peer);
            notify(
                event_tx,
                SidecarEvent::ProviderDiscovered {
                    peer_id: peer.clone(),
                },
            );
            changed = true;
        }
    }
    if changed {
        log::debug!(
            "sidecar provider cache updated count={}",
            state.known_providers.len()
        );
    }
}

/// Records that providers were discovered under the legacy `podmesh-proxy-node` key.
/// Per the sidecar-proxy-auth spec these peers MUST NOT be used for workload traffic;
/// we keep this only as a debug signal and to emit `ProxyPeerDiscovered` events that
/// existing tests rely on for their assertions about generic proxy availability.
fn update_provider_cache_for_proxy<I>(
    providers: I,
    _state: &mut SidecarState,
    event_tx: Option<&mpsc::UnboundedSender<SidecarEvent>>,
) where
    I: IntoIterator<Item = libp2p::PeerId>,
{
    for peer in providers {
        log::debug!(
            "sidecar saw legacy podmesh-proxy-node provider peer={} (NOT used for workload traffic)",
            peer
        );
        notify(
            event_tx,
            SidecarEvent::ProxyPeerDiscovered {
                peer_id: peer.to_string(),
            },
        );
    }
}

/// Updates the list of tenant-derived proxy peers (discovered via the obfuscated
/// `blake3(owner_pubkey)[..16]` DHT key). These are the only peers eligible to
/// be used for egress tunneling and SidecarRegistration, and only after their
/// `NodeCert` has been verified during handshake.
///
/// For each newly-discovered tenant proxy peer we explicitly send an outbound
/// handshake request so that the proxy responds with its `proxy_cert_b64`, even
/// if our local handshake state already considers the peer "confirmed" because
/// it received an inbound handshake request first. The cert is only carried in
/// the response payload.
fn update_tenant_proxy_peers<I>(
    swarm: &mut Swarm<SidecarBehaviour>,
    cfg: &SidecarConfig,
    providers: I,
    state: &mut SidecarState,
    event_tx: Option<&mpsc::UnboundedSender<SidecarEvent>>,
) where
    I: IntoIterator<Item = libp2p::PeerId>,
{
    let new_peers: Vec<PeerId> = providers.into_iter().collect();
    if new_peers.is_empty() {
        log::debug!("no tenant proxy providers found");
    } else {
        log::info!(
            "discovered {} tenant proxy provider(s): {:?}",
            new_peers.len(),
            new_peers
        );
        for peer in &new_peers {
            notify(
                event_tx,
                SidecarEvent::ProxyPeerDiscovered {
                    peer_id: peer.to_string(),
                },
            );
        }
        state.proxy_peers = new_peers.clone();
        for peer in new_peers {
            // Explicitly request a handshake from the proxy so we receive a Response
            // carrying the proxy_cert_b64. Without this, the drive ticker may have
            // already marked the peer as confirmed because the proxy initiated.
            send_handshake_request_to_proxy(swarm, &peer);

            // If the cert was already verified earlier (rare but possible), trigger
            // registration immediately.
            if state.verified_proxy_peers.contains(&peer) && state.pending_registration_id.is_none()
            {
                send_sidecar_registration(swarm, cfg, peer, state);
            }
        }
    }
    state.proxy_query_pending = false;
}

/// Build and send a single signed handshake request to a peer, bypassing the
/// drive-throttling logic. Used to force a Response from a tenant proxy so we
/// can extract its `proxy_cert_b64`.
fn send_handshake_request_to_proxy(swarm: &mut Swarm<SidecarBehaviour>, peer: &PeerId) {
    let local_peer = *swarm.local_peer_id();
    match handshake::build_handshake_request_for_kem_fetch(&local_peer) {
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

/// Triggers a DHT lookup for proxy providers — DEPRECATED.
/// Per the sidecar-proxy-auth spec, sidecars MUST NOT discover proxies for
/// workload traffic via the global `podmesh-proxy-node` key. Use
/// [`trigger_tenant_proxy_lookup`] instead. This stub is retained only as
/// documentation and a compile-time anchor for callers that may have been missed.
#[allow(dead_code)]
fn trigger_proxy_lookup(_swarm: &mut Swarm<SidecarBehaviour>, _state: &mut SidecarState) {
    log::warn!(
        "trigger_proxy_lookup called: this is a deprecated no-op; use trigger_tenant_proxy_lookup"
    );
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
        Ok(ma) => {
            register_kad_peer(swarm, &ma);
            dial_multiaddr(swarm, &ma);
        }
        Err(err) => log::warn!("invalid bootstrap multiaddr addr={} error={}", addr, err),
    }
}

fn dial_multiaddr(swarm: &mut Swarm<SidecarBehaviour>, addr: &Multiaddr) {
    if let Err(err) = swarm.dial(addr.clone()) {
        log::warn!(
            "sidecar failed to dial bootstrap peer addr={} error={}",
            addr,
            err
        );
    } else {
        log::debug!("sidecar dialing bootstrap peer addr={}", addr);
    }
}

fn notify(event_tx: Option<&mpsc::UnboundedSender<SidecarEvent>>, event: SidecarEvent) {
    if let Some(tx) = event_tx
        && let Err(err) = tx.send(event)
    {
        log::warn!("sidecar failed to emit event error={}", err);
    }
}

fn register_kad_peer(swarm: &mut Swarm<SidecarBehaviour>, addr: &Multiaddr) {
    if let Some((peer_id, base_addr)) = split_peer_multiaddr(addr) {
        swarm
            .behaviour_mut()
            .kademlia
            .add_address(&peer_id, base_addr);
    }
}

fn split_peer_multiaddr(addr: &Multiaddr) -> Option<(PeerId, Multiaddr)> {
    let mut base = addr.clone();
    match base.pop() {
        Some(Protocol::P2p(peer_id)) => Some((peer_id, base)),
        _ => None,
    }
}

// Re-export split_csv from shared p2p crate
pub use p2p::split_csv;

// ── Phase 7.3: sidecar→proxy registration ──────────────────────────────────

/// Compute the opaque DHT key for the tenant proxy: `blake3(owner_pubkey)[..16]`.
/// Re-exports the canonical implementation from the `protocol` crate.
pub fn compute_tenant_proxy_dht_key(owner_pubkey_b64: &str) -> anyhow::Result<Vec<u8>> {
    protocol::compute_tenant_proxy_dht_key(owner_pubkey_b64)
}

/// Trigger a DHT lookup for the tenant proxy (opaque key based on owner pubkey).
/// Called both on initial bootstrap completion and on the periodic ticker for refresh.
fn trigger_tenant_proxy_lookup(
    swarm: &mut Swarm<SidecarBehaviour>,
    cfg: &SidecarConfig,
    state: &mut SidecarState,
) {
    let Some(ref owner_pub) = cfg.owner_public_key_b64 else {
        log::debug!("sidecar has no owner pubkey, skipping tenant proxy lookup");
        return;
    };
    if state.proxy_query_pending {
        log::debug!("tenant proxy provider query already pending, skipping");
        return;
    }
    match compute_tenant_proxy_dht_key(owner_pub) {
        Ok(key) => {
            let record_key = RecordKey::new(&key);
            let query_id = swarm.behaviour_mut().kademlia.get_providers(record_key);
            state.tenant_proxy_lookup_triggered = true;
            state.proxy_query_pending = true;
            log::info!(
                "sidecar triggered tenant proxy DHT lookup query_id={:?}",
                query_id
            );
        }
        Err(err) => {
            log::warn!("failed to compute tenant proxy DHT key: {}", err);
        }
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
    state.pending_registration_id = Some(request_id);
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
                if Some(request_id) == state.pending_registration_id {
                    state.pending_registration_id = None;
                }
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
        request_response::Event::OutboundFailure { peer, error, .. } => {
            log::warn!(
                "sidecar registration outbound failure peer={} error={:?}",
                peer,
                error
            );
            state.pending_registration_id = None;
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
