use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{StreamExt, future};
use libp2p::{
    Multiaddr, PeerId, Swarm,
    kad::{self, RecordKey},
    multiaddr::Protocol,
    request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use p2p::{
    handshake::{self, HandshakeDriveConfig, HandshakeState},
    request_response::ByteCodec,
};
use tokio::signal;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

type HandshakeCodec = ByteCodec;

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
    info!(has_events = event_tx.is_some(), "gateway runtime starting");
    let mut swarm = build_swarm(&cfg)?;
    if let Some(addr) = cfg.listen_addr() {
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

    let mut state = GatewayState::default();
    let mut handshake_ticker = tokio::time::interval(Duration::from_secs(1));
    handshake_ticker.tick().await;
    loop {
        tokio::select! {
            event = swarm.select_next_some() => {
                handle_swarm_event(&mut swarm, event, &cfg, &mut state, event_tx.as_ref())
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
                drive_handshakes(&mut swarm, &mut state);
            }
        }
    }

    Ok(())
}

impl GatewayConfig {
    pub fn record_key(&self) -> RecordKey {
        RecordKey::new(&self.provider_label)
    }

    pub fn listen_addr(&self) -> Option<Multiaddr> {
        match self.libp2p_host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ipv4)) => {
                let mut addr = Multiaddr::empty();
                addr.push(Protocol::Ip4(ipv4));
                addr.push(Protocol::Udp(self.libp2p_port));
                addr.push(Protocol::QuicV1);
                Some(addr)
            }
            Ok(IpAddr::V6(ipv6)) => {
                let mut addr = Multiaddr::empty();
                addr.push(Protocol::Ip6(ipv6));
                addr.push(Protocol::Udp(self.libp2p_port));
                addr.push(Protocol::QuicV1);
                Some(addr)
            }
            Err(_) => format!("/ip4/{}/udp/{}/quic-v1", self.libp2p_host, self.libp2p_port)
                .parse()
                .ok(),
        }
    }

    pub fn bootstrap_peer_multiaddr(&self) -> Option<Multiaddr> {
        let raw = self.bootstrap_peer_ip.as_deref()?;

        if let Ok(ma) = raw.parse::<Multiaddr>() {
            return Some(ma);
        }

        let (host, port_opt) = match raw.rsplit_once(':') {
            Some((host, port_str)) => match port_str.parse::<u16>() {
                Ok(port) => (host, Some(port)),
                Err(err) => {
                    warn!(input = %raw, error = %err, "invalid bootstrap port");
                    return None;
                }
            },
            None => (raw, None),
        };

        let port = match port_opt.or_else(|| (self.libp2p_port != 0).then_some(self.libp2p_port)) {
            Some(port) => port,
            None => {
                warn!(input = %raw, "bootstrap address missing port and no libp2p_port provided");
                return None;
            }
        };

        match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ipv4)) => {
                let mut addr = Multiaddr::empty();
                addr.push(Protocol::Ip4(ipv4));
                addr.push(Protocol::Udp(port));
                addr.push(Protocol::QuicV1);
                Some(addr)
            }
            Ok(IpAddr::V6(ipv6)) => {
                let mut addr = Multiaddr::empty();
                addr.push(Protocol::Ip6(ipv6));
                addr.push(Protocol::Udp(port));
                addr.push(Protocol::QuicV1);
                Some(addr)
            }
            Err(err) => {
                warn!(input = %raw, error = %err, "invalid bootstrap ip");
                None
            }
        }
    }
}

#[derive(NetworkBehaviour)]
struct GatewayBehaviour {
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    handshake_rr: request_response::Behaviour<HandshakeCodec>,
}

#[derive(Default)]
struct GatewayState {
    known_providers: HashSet<String>,
    handshake_states: HashMap<PeerId, HandshakeState>,
    kad_bootstrapped: bool,
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
            let mut behaviour = GatewayBehaviour {
                kademlia: kad::Behaviour::new(peer_id, store),
                handshake_rr,
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

fn handle_swarm_event(
    swarm: &mut Swarm<GatewayBehaviour>,
    event: SwarmEvent<GatewayBehaviourEvent>,
    cfg: &GatewayConfig,
    state: &mut GatewayState,
    event_tx: Option<&mpsc::UnboundedSender<GatewayEvent>>,
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

fn drive_handshakes(swarm: &mut Swarm<GatewayBehaviour>, state: &mut GatewayState) {
    let local_peer = swarm.local_peer_id().clone();
    let mut pending: Vec<(PeerId, Vec<u8>)> = Vec::new();
    let mut dropped: Vec<PeerId> = Vec::new();
    if let Err(err) = handshake::drive_handshakes(
        &mut state.handshake_states,
        &local_peer,
        &HandshakeDriveConfig::default(),
        |peer, payload| {
            pending.push((peer.clone(), payload));
            true
        },
        |peer| dropped.push(peer.clone()),
    ) {
        warn!(?err, "gateway handshake drive failed");
    }

    for (peer, payload) in pending {
        let request_id = swarm
            .behaviour_mut()
            .handshake_rr
            .send_request(&peer, payload);
        debug!(%peer, ?request_id, "gateway handshake request sent");
    }

    for peer in dropped {
        debug!(%peer, "gateway removing peer after failed handshake attempts");
        swarm.behaviour_mut().kademlia.remove_peer(&peer);
    }
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

pub fn split_csv(input: Option<String>) -> Vec<String> {
    input
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}
