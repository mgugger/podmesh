use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::{StreamExt, future};
use libp2p::{
    Multiaddr, PeerId, Swarm,
    kad::{self, Quorum, Record, RecordKey},
    multiaddr::Protocol,
    request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use p2p::{build_quic_multiaddr, parse_bootstrap_peer, timestamp_millis};
use p2p::http_proxy::{ProxyCodec, ProxyHttpRequest, ProxyHttpResponse};
use p2p::{
    handshake::{self, HandshakeDriveConfig, HandshakeState},
    request_response::HandshakeCodec,
};
use protocol::libp2p_constants::{INGRESS_PROXY_PROTOCOL, MANIFEST_RECORD_PREFIX};
use protocol::machine::{GatewayRouteSpec, build_gateway_provider_record};
use reqwest::{
    Client, Method,
    header::{HeaderName, HeaderValue},
};
use tokio::signal;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

pub mod manifest_routes;

pub const DEFAULT_GATEWAY_APP_PORT: u16 = 18080;
const MANIFEST_RECORD_TTL_MS: u32 = 30_000;
const MANIFEST_RECORD_VERSION: u16 = 1;

#[derive(Clone, Debug)]
pub struct GatewayConfig {
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
    pub routes: Vec<GatewayRouteSpec>,
    pub owner_public_key_b64: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    Connected { peer_id: String },
    ProviderDiscovered { peer_id: String },
}

pub async fn run_gateway(cfg: GatewayConfig) -> Result<()> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        match signal::ctrl_c().await {
            Ok(_) => {
                let _ = shutdown_tx.send(());
            }
            Err(err) => {
                warn!(error = %err, "gateway ctrl+c listener failed");
                let _ = shutdown_tx.send(());
            }
        }
    });
    run_gateway_with_shutdown(cfg, shutdown_rx, None).await
}

pub async fn run_gateway_with_shutdown(
    cfg: GatewayConfig,
    mut shutdown: oneshot::Receiver<()>,
    event_tx: Option<mpsc::UnboundedSender<GatewayEvent>>,
) -> Result<()> {
    let listen_addr = cfg.listen_addr();
    let listen_addr_display = listen_addr
        .as_ref()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "none".to_string());
    info!(
        has_events = event_tx.is_some(),
        provider = %cfg.provider_label,
        manifest = %cfg.manifest_id,
        ingress_host = %cfg.ingress_host,
        libp2p_host = %cfg.libp2p_host,
        libp2p_port = cfg.libp2p_port,
        announce_providers = cfg.announce_providers,
        lookup_interval_ms = cfg.lookup_interval.as_millis() as u64,
        announce_interval_ms = cfg.announce_interval.as_millis() as u64,
        bootstrap_peers = ?cfg.bootstrap_peers,
        bootstrap_peer_ip = %cfg.bootstrap_peer_ip.as_deref().unwrap_or("none"),
        listen_addr = %listen_addr_display,
        app_port = cfg.app_port,
        routes = cfg.routes.len(),
        "gateway runtime starting with config"
    );

    let mut swarm = build_swarm(&cfg)?;
    if let Some(addr) = listen_addr {
        swarm
            .listen_on(addr)
            .context("start gateway libp2p listener")?;
    }

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
        .context("build gateway http client")?;
    let mut state = GatewayState::new(http_client);
    let (proxy_resp_tx, mut proxy_resp_rx) = mpsc::unbounded_channel();
    let mut handshake_ticker = tokio::time::interval(Duration::from_secs(1));
    handshake_ticker.tick().await;
    let mut manifest_ticker = tokio::time::interval(cfg.announce_interval);
    manifest_ticker.tick().await;
    publish_manifest_record(&mut swarm, &cfg);
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
                if cfg.announce_providers {
                    announce_provider(&mut swarm, &cfg);
                }
            },
            _ = &mut shutdown => {
                info!("gateway shutdown requested");
                break;
            }
            _ = handshake_ticker.tick() => {
                let local_peer = swarm.local_peer_id().clone();
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
                            debug!(%peer, ?request_id, "gateway handshake request sent");
                        }

                        for peer in actions.drops {
                            debug!(%peer, "gateway removing peer after failed handshake attempts");
                            swarm.behaviour_mut().kademlia.remove_peer(&peer);
                        }
                    }
                    Err(err) => {
                        warn!(?err, "gateway handshake drive failed");
                    }
                }
            },
            _ = manifest_ticker.tick() => {
                publish_manifest_record(&mut swarm, &cfg);
            },
            Some(pending) = proxy_resp_rx.recv() => {
                if let Err(err) = swarm.behaviour_mut().proxy_rr.send_response(pending.channel, pending.response) {
                    warn!(error = ?err, "failed to send proxy response to workload");
                }
            }
        }
    }

    Ok(())
}

impl GatewayConfig {
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
struct GatewayBehaviour {
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    handshake_rr: request_response::Behaviour<HandshakeCodec>,
    proxy_rr: request_response::Behaviour<ProxyCodec>,
}

struct GatewayState {
    known_providers: HashSet<String>,
    handshake_states: HashMap<PeerId, HandshakeState>,
    kad_bootstrapped: bool,
    http_client: Client,
}

impl GatewayState {
    fn new(http_client: Client) -> Self {
        Self {
            known_providers: HashSet::new(),
            handshake_states: HashMap::new(),
            kad_bootstrapped: false,
            http_client,
        }
    }
}

struct PendingProxyResponse {
    channel: request_response::ResponseChannel<ProxyHttpResponse>,
    response: ProxyHttpResponse,
}

fn build_swarm(_cfg: &GatewayConfig) -> Result<Swarm<GatewayBehaviour>> {
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
            let mut behaviour = GatewayBehaviour {
                kademlia: kad::Behaviour::new(peer_id, store),
                handshake_rr,
                proxy_rr,
            };
            behaviour.kademlia.set_mode(Some(kad::Mode::Client));
            behaviour
        })?
        .build();

    debug!("local gateway peer_id={}", swarm.local_peer_id());
    let kad_protocols: Vec<String> = swarm
        .behaviour()
        .kademlia
        .protocol_names()
        .iter()
        .map(|p| p.to_string())
        .collect();
    debug!(?kad_protocols, "gateway kad protocols configured");

    Ok(swarm)
}

fn dial_bootstrap(swarm: &mut Swarm<GatewayBehaviour>, cfg: &GatewayConfig) {
    for addr in &cfg.bootstrap_peers {
        dial_multiaddr_str(swarm, addr);
    }

    if let Some(addr) = cfg.bootstrap_peer_multiaddr() {
        dial_multiaddr(swarm, &addr);
    } else if cfg.bootstrap_peer_ip.is_some() {
        warn!(
            ip = cfg.bootstrap_peer_ip.as_deref().unwrap_or(""),
            port = cfg.libp2p_port,
            "gateway bootstrap ip provided but no valid multiaddr could be built"
        );
    }
}

fn trigger_lookup(swarm: &mut Swarm<GatewayBehaviour>, cfg: &GatewayConfig) {
    let key = cfg.record_key();
    let query_id = swarm.behaviour_mut().kademlia.get_providers(key);
    debug!(?query_id, provider = %cfg.provider_label, "gateway started provider lookup");
}

fn announce_provider(swarm: &mut Swarm<GatewayBehaviour>, cfg: &GatewayConfig) {
    let key = cfg.record_key();
    match swarm.behaviour_mut().kademlia.start_providing(key) {
        Ok(query_id) => {
            debug!(?query_id, provider = %cfg.provider_label, "gateway announced provider")
        }
        Err(err) => {
            warn!(provider = %cfg.provider_label, error = %err, "gateway provider announce failed")
        }
    }
}

fn publish_manifest_record(swarm: &mut Swarm<GatewayBehaviour>, cfg: &GatewayConfig) {
    let timestamp_ms = timestamp_millis();
    let payload = build_gateway_provider_record(
        &cfg.manifest_id,
        &swarm.local_peer_id().to_string(),
        &cfg.ingress_host,
        cfg.owner_public_key_b64.as_deref(),
        &cfg.routes,
        MANIFEST_RECORD_TTL_MS,
        timestamp_ms,
        MANIFEST_RECORD_VERSION,
    );

    let record = Record {
        key: cfg.manifest_record_key(),
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
            debug!(?query_id, manifest = %cfg.manifest_id, "gateway published manifest record")
        }
        Err(err) => {
            warn!(manifest = %cfg.manifest_id, error = %err, "gateway failed to publish manifest record")
        }
    }
}

fn handle_swarm_event(
    swarm: &mut Swarm<GatewayBehaviour>,
    event: SwarmEvent<GatewayBehaviourEvent>,
    cfg: &GatewayConfig,
    state: &mut GatewayState,
    event_tx: Option<&mpsc::UnboundedSender<GatewayEvent>>,
    proxy_resp_tx: &mpsc::UnboundedSender<PendingProxyResponse>,
) {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => {
            info!(%address, "gateway listening for libp2p peers");
        }
        SwarmEvent::ConnectionEstablished {
            peer_id, endpoint, ..
        } => {
            debug!(%peer_id, "gateway connection established");
            handshake::track_peer(&mut state.handshake_states, &peer_id);
            let addr = endpoint.get_remote_address().clone();
            swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);

            if !state.kad_bootstrapped {
                match swarm.behaviour_mut().kademlia.bootstrap() {
                    Ok(_) => {
                        state.kad_bootstrapped = true;
                        debug!("gateway initiated kademlia bootstrap");
                    }
                    Err(err) => {
                        debug!(%err, "gateway kademlia bootstrap attempt failed");
                    }
                }
            }

            notify(
                event_tx,
                GatewayEvent::Connected {
                    peer_id: peer_id.to_string(),
                },
            );
        }
        SwarmEvent::ConnectionClosed {
            peer_id,
            num_established,
            ..
        } => {
            debug!(%peer_id, "gateway connection closed");
            handshake::untrack_peer(&mut state.handshake_states, &peer_id);
            if num_established == 0 {
                swarm.behaviour_mut().kademlia.remove_peer(&peer_id);
            }
        }
        SwarmEvent::Behaviour(GatewayBehaviourEvent::Kademlia(event)) => {
            handle_kad_event(event, cfg, state, event_tx);
        }
        SwarmEvent::Behaviour(GatewayBehaviourEvent::HandshakeRr(event)) => {
            handle_handshake_event(swarm, event, state);
        }
        SwarmEvent::Behaviour(GatewayBehaviourEvent::ProxyRr(event)) => {
            handle_proxy_event(cfg, state, event, proxy_resp_tx);
        }
        _ => {}
    }
}

fn handle_handshake_event(
    swarm: &mut Swarm<GatewayBehaviour>,
    event: request_response::Event<Vec<u8>, Vec<u8>>,
    state: &mut GatewayState,
) {
    match event {
        request_response::Event::Message { message, peer, .. } => {
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
            warn!(%peer, ?error, "gateway handshake outbound failure");
            if matches!(
                error,
                request_response::OutboundFailure::UnsupportedProtocols
            ) {
                handshake::track_peer(&mut state.handshake_states, &peer).confirmed = true;
                debug!(%peer, "gateway treating peer as handshake-confirmed due to unsupported protocol");
            }
        }
        request_response::Event::InboundFailure { peer, error, .. } => {
            warn!(%peer, ?error, "gateway handshake inbound failure");
        }
        request_response::Event::ResponseSent { peer, .. } => {
            debug!(%peer, "gateway handshake response sent");
        }
    }
}

fn handle_proxy_event(
    cfg: &GatewayConfig,
    state: &GatewayState,
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
                    %peer,
                    manifest = %request.manifest_id,
                    method = %request.method,
                    path = %request.path_and_query,
                    target_port = request.target_port,
                    "gateway received proxy request"
                );
                spawn_local_http_request(
                    state.http_client.clone(),
                    request,
                    channel,
                    proxy_resp_tx.clone(),
                );
            }
            request_response::Message::Response { response, .. } => {
                debug!(%peer, status = response.status_code, "gateway received proxy response acknowledgement");
            }
        },
        request_response::Event::OutboundFailure { peer, error, .. } => {
            warn!(%peer, ?error, "gateway proxy outbound failure");
        }
        request_response::Event::InboundFailure { peer, error, .. } => {
            warn!(%peer, ?error, "gateway proxy inbound failure");
        }
        request_response::Event::ResponseSent { peer, .. } => {
            debug!(%peer, "gateway proxy response sent");
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
                info!(
                    manifest = %manifest_id,
                    method = %method,
                    path = %path,
                    target_port,
                    status = resp.status_code,
                    "gateway forwarded request to application"
                );
                resp
            }
            Err(err) => {
                warn!(
                    manifest = %manifest_id,
                    method = %method,
                    path = %path,
                    target_port,
                    error = ?err,
                    "gateway local http request failed"
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
    event: kad::Event,
    cfg: &GatewayConfig,
    state: &mut GatewayState,
    event_tx: Option<&mpsc::UnboundedSender<GatewayEvent>>,
) {
    match event {
        kad::Event::OutboundQueryProgressed { result, .. } => match result {
            kad::QueryResult::GetProviders(Ok(ok)) => match ok {
                kad::GetProvidersOk::FoundProviders { key, providers } => {
                    debug!(key = ?key, expected = ?cfg.record_key(), count = providers.len(), "gateway get_providers result");
                    if key == cfg.record_key() {
                        update_provider_cache(providers, state, event_tx);
                    }
                }
                kad::GetProvidersOk::FinishedWithNoAdditionalRecord { closest_peers } => {
                    debug!(
                        provider = %cfg.provider_label,
                        closest = closest_peers.len(),
                        "gateway provider lookup finished without providers"
                    );
                }
            },
            kad::QueryResult::GetProviders(Err(err)) => {
                warn!(provider = %cfg.provider_label, error = %err, "gateway provider lookup failed");
            }
            _ => {}
        },
        kad::Event::RoutingUpdated { peer, .. } => {
            debug!(%peer, "gateway routing entry updated");
        }
        _ => {}
    }
}

fn update_provider_cache<I>(
    providers: I,
    state: &mut GatewayState,
    event_tx: Option<&mpsc::UnboundedSender<GatewayEvent>>,
) where
    I: IntoIterator<Item = libp2p::PeerId>,
{
    let mut changed = false;
    for id in providers {
        let peer = id.to_string();
        if state.known_providers.insert(peer.clone()) {
            info!(%peer, "gateway discovered provider");
            notify(
                event_tx,
                GatewayEvent::ProviderDiscovered {
                    peer_id: peer.clone(),
                },
            );
            changed = true;
        }
    }
    if changed {
        debug!(
            count = state.known_providers.len(),
            "gateway provider cache updated"
        );
    }
}

fn dial_multiaddr_str(swarm: &mut Swarm<GatewayBehaviour>, addr: &str) {
    match addr.parse::<Multiaddr>() {
        Ok(ma) => {
            register_kad_peer(swarm, &ma);
            dial_multiaddr(swarm, &ma);
        }
        Err(err) => warn!(%addr, error = %err, "invalid bootstrap multiaddr"),
    }
}

fn dial_multiaddr(swarm: &mut Swarm<GatewayBehaviour>, addr: &Multiaddr) {
    if let Err(err) = swarm.dial(addr.clone()) {
        warn!(%addr, error = %err, "gateway failed to dial bootstrap peer");
    } else {
        debug!(%addr, "gateway dialing bootstrap peer");
    }
}

fn notify(event_tx: Option<&mpsc::UnboundedSender<GatewayEvent>>, event: GatewayEvent) {
    if let Some(tx) = event_tx {
        if let Err(err) = tx.send(event) {
            warn!(error = %err, "gateway failed to emit event");
        }
    }
}

fn register_kad_peer(swarm: &mut Swarm<GatewayBehaviour>, addr: &Multiaddr) {
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

