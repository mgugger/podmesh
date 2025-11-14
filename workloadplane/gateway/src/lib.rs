use std::collections::HashSet;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::{
    Multiaddr, Swarm,
    kad::{self, RecordKey},
    multiaddr::Protocol,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use tokio::signal;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub provider_label: String,
    pub bootstrap_peers: Vec<String>,
    pub bootstrap_peer_ip: Option<String>,
    pub lookup_interval: Duration,
    pub announce_interval: Duration,
    pub libp2p_host: String,
    pub libp2p_port: u16,
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

    let mut announce_ticker = tokio::time::interval(cfg.announce_interval);
    announce_ticker.tick().await;
    announce_provider(&mut swarm, &cfg);

    let mut state = GatewayState::default();
    loop {
        tokio::select! {
            event = swarm.select_next_some() => handle_swarm_event(event, &cfg, &mut state, event_tx.as_ref()),
            _ = lookup_ticker.tick() => trigger_lookup(&mut swarm, &cfg),
            _ = announce_ticker.tick() => announce_provider(&mut swarm, &cfg),
            _ = &mut shutdown => {
                info!("gateway shutdown requested");
                break;
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
}

#[derive(Default)]
struct GatewayState {
    known_providers: HashSet<String>,
}

fn build_swarm(_cfg: &GatewayConfig) -> Result<Swarm<GatewayBehaviour>> {
    let swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_quic()
        .with_dns()?
        .with_behaviour(|key| {
            let peer_id = key.public().to_peer_id();
            let store = kad::store::MemoryStore::new(peer_id);
            let mut behaviour = GatewayBehaviour {
                kademlia: kad::Behaviour::new(peer_id, store),
            };
            behaviour.kademlia.set_mode(Some(kad::Mode::Server));
            behaviour
        })?
        .build();

    debug!("local gateway peer_id={}", swarm.local_peer_id());

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
    event: SwarmEvent<GatewayBehaviourEvent>,
    cfg: &GatewayConfig,
    state: &mut GatewayState,
    event_tx: Option<&mpsc::UnboundedSender<GatewayEvent>>,
) {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => {
            info!(%address, "gateway listening for libp2p peers");
        }
        SwarmEvent::ConnectionEstablished { peer_id, .. } => {
            debug!(%peer_id, "gateway connection established");
            notify(
                event_tx,
                GatewayEvent::Connected {
                    peer_id: peer_id.to_string(),
                },
            );
        }
        SwarmEvent::ConnectionClosed { peer_id, .. } => {
            debug!(%peer_id, "gateway connection closed");
        }
        SwarmEvent::Behaviour(GatewayBehaviourEvent::Kademlia(event)) => {
            handle_kad_event(event, cfg, state, event_tx);
        }
        _ => {}
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
                    if key == cfg.record_key() {
                        update_provider_cache(providers, state, event_tx);
                    }
                }
                kad::GetProvidersOk::FinishedWithNoAdditionalRecord { .. } => {
                    debug!(provider = %cfg.provider_label, "gateway provider lookup finished");
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
        Ok(ma) => dial_multiaddr(swarm, &ma),
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

pub fn split_csv(input: Option<String>) -> Vec<String> {
    input
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}
