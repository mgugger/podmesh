mod connection;
mod streams;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, EndpointId, RelayMode, endpoint::presets, tls::CaTlsConfig};
use protocol::EndpointRecord;
use reqwest::Client;
use rustls_pki_types::CertificateDer;
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{SidecarConfig, SidecarEvent, egress_nft, egress_proxy, http_connect_proxy};
use connection::ProxySession;

const MAX_PROXY_CANDIDATES: usize = 32;
const MAX_PARALLEL_CONNECTION_ATTEMPTS: usize = 8;
const MAX_CONCURRENT_INCOMING_STREAMS: usize = 256;
const PROXY_RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
const REGISTRATION_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run(
    config: SidecarConfig,
    mut shutdown: oneshot::Receiver<()>,
    event_tx: Option<mpsc::UnboundedSender<SidecarEvent>>,
) -> Result<()> {
    config.validate()?;
    let endpoint = bind_endpoint(&config).await?;
    let cancellation = CancellationToken::new();
    let http_client = Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .context("build sidecar HTTP client")?;
    let stream_slots = Arc::new(Semaphore::new(MAX_CONCURRENT_INCOMING_STREAMS));
    let connect_slots = Arc::new(Semaphore::new(MAX_PARALLEL_CONNECTION_ATTEMPTS));
    let (candidate_tx, mut candidate_rx) = mpsc::channel::<EndpointRecord>(MAX_PROXY_CANDIDATES);
    let (connected_tx, mut connected_rx) = mpsc::channel(MAX_PROXY_CANDIDATES);
    let (disconnected_tx, mut disconnected_rx) = mpsc::channel(MAX_PROXY_CANDIDATES);
    let (tunnel_tx, mut tunnel_rx) = mpsc::channel(256);
    let (egress_cleanup, listener_tasks) = start_local_proxies(&config, tunnel_tx)?;

    let mut candidates = HashMap::<EndpointId, EndpointRecord>::new();
    let mut connecting = HashSet::<EndpointId>::new();
    let mut sessions = HashMap::<EndpointId, ProxySession>::new();
    for record in config.proxy_endpoints.clone() {
        candidate_tx
            .try_send(record)
            .map_err(|_| anyhow::anyhow!("initial proxy candidate queue is full"))?;
    }
    let config = Arc::new(config);
    let mut reconnect = tokio::time::interval(PROXY_RECONNECT_INTERVAL);
    let mut registration = tokio::time::interval(REGISTRATION_REFRESH_INTERVAL);
    let mut discovery = tokio::time::interval(config.lookup_interval);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            Some(record) = candidate_rx.recv() => {
                let address = iroh_support::endpoint_addr(&record, now_secs())?;
                let id = address.id;
                if !candidates.contains_key(&id) && candidates.len() >= MAX_PROXY_CANDIDATES {
                    log::warn!("discarding proxy candidate because candidate limit is reached");
                    continue;
                }
                candidates.insert(id, record.clone());
                if !sessions.contains_key(&id) && connecting.insert(id) {
                    spawn_connection_attempt(
                        endpoint.clone(),
                        config.clone(),
                        record,
                        cancellation.clone(),
                        connect_slots.clone(),
                        connected_tx.clone(),
                    );
                }
            }
            Some((record, result)) = connected_rx.recv() => {
                let id = endpoint_id(&record)?;
                connecting.remove(&id);
                match result {
                    Ok(session) => {
                        if let Some(previous) = sessions.insert(id, session.clone()) {
                            previous.connection.close(2u8.into(), b"proxy session replaced");
                        }
                        notify(&event_tx, SidecarEvent::Connected { peer_id: id.to_string() });
                        notify(&event_tx, SidecarEvent::ProxyPeerDiscovered { peer_id: id.to_string() });
                        if session.verified
                            && let Err(error) = connection::register(endpoint.id(), &config, &session, &cancellation).await
                        {
                            log::warn!("initial sidecar registration failed endpoint={} error={error}", id.fmt_short());
                        }
                        tokio::spawn(streams::serve_connection(
                            session.connection.clone(),
                            config.clone(),
                            http_client.clone(),
                            stream_slots.clone(),
                            cancellation.clone(),
                            disconnected_tx.clone(),
                        ));
                    }
                    Err(error) => log::warn!("proxy connection failed endpoint={} error={error}", id.fmt_short()),
                }
            }
            Some(id) = disconnected_rx.recv() => {
                if let Some(session) = sessions.remove(&id) {
                    candidates.insert(id, session.record);
                }
            }
            _ = reconnect.tick() => {
                for (id, record) in candidates.clone() {
                    if !sessions.contains_key(&id) && connecting.insert(id) {
                        spawn_connection_attempt(
                            endpoint.clone(),
                            config.clone(),
                            record,
                            cancellation.clone(),
                            connect_slots.clone(),
                            connected_tx.clone(),
                        );
                    }
                }
            }
            _ = registration.tick() => {
                for session in sessions.values() {
                    if session.verified
                        && let Err(error) = connection::register(endpoint.id(), &config, session, &cancellation).await
                    {
                        log::warn!("sidecar registration refresh failed endpoint={} error={error}", session.connection.remote_id().fmt_short());
                    }
                }
            }
            _ = discovery.tick(), if sessions.values().any(|session| session.verified) => {
                if let Some(session) = sessions.values().find(|session| session.verified) {
                    match connection::discover(&config, session, &cancellation).await {
                        Ok(records) => {
                            for record in records {
                                if candidate_tx.try_send(record).is_err() {
                                    log::warn!("proxy candidate queue is full");
                                    break;
                                }
                            }
                        }
                        Err(error) => log::warn!("proxy discovery failed: {error}"),
                    }
                }
            }
            Some(tunnel) = tunnel_rx.recv() => {
                if let Some(session) = sessions.values().find(|session| session.verified) {
                    tokio::spawn(streams::open_egress(
                        session.connection.clone(),
                        tunnel,
                        cancellation.clone(),
                        event_tx.clone(),
                    ));
                } else {
                    notify(&event_tx, SidecarEvent::EgressTunnelFailed {
                        dest_host: tunnel.dest_host,
                        dest_port: tunnel.dest_port,
                        error: "no verified proxy connection".into(),
                    });
                }
            }
        }
    }

    cancellation.cancel();
    endpoint.close().await;
    for task in listener_tasks {
        task.abort();
        let _ = task.await;
    }
    if egress_cleanup && let Err(error) = egress_nft::cleanup_egress_rules() {
        log::warn!("failed to clean up sidecar egress rules: {error}");
    }
    Ok(())
}

async fn bind_endpoint(config: &SidecarConfig) -> Result<Endpoint> {
    let secret = config.identity.load()?;
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .bind_addr(config.iroh_bind_addr)?;
    let relay_urls = config
        .proxy_endpoints
        .iter()
        .filter_map(|record| record.relay_url.as_deref())
        .collect::<HashSet<_>>();
    if relay_urls.is_empty() {
        builder = builder.clear_relay_transports();
    } else {
        let token = config
            .workload_relay_auth_token
            .as_ref()
            .context("workload relay auth token is required")?;
        let map = iroh::RelayMap::try_from_iter(relay_urls)?;
        let authenticated = map
            .relays::<Vec<_>>()
            .into_iter()
            .map(|relay| relay.as_ref().clone().with_auth_token(token.clone()))
            .collect();
        builder = builder.relay_mode(RelayMode::Custom(authenticated));
        if !config.workload_relay_ca_certificates.is_empty() {
            builder = builder.ca_tls_config(
                CaTlsConfig::embedded().with_extra_roots(
                    config
                        .workload_relay_ca_certificates
                        .iter()
                        .cloned()
                        .map(CertificateDer::from),
                ),
            );
        }
    }
    builder.bind().await.context("bind sidecar Iroh endpoint")
}

fn spawn_connection_attempt(
    endpoint: Endpoint,
    config: Arc<SidecarConfig>,
    record: EndpointRecord,
    cancellation: CancellationToken,
    slots: Arc<Semaphore>,
    sender: mpsc::Sender<(EndpointRecord, Result<ProxySession>)>,
) {
    tokio::spawn(async move {
        let result = match slots.acquire_owned().await {
            Ok(_permit) => {
                connection::connect(&endpoint, &config, record.clone(), &cancellation).await
            }
            Err(_) => Err(anyhow::anyhow!("proxy connection limiter closed")),
        };
        let _ = sender.send((record, result)).await;
    });
}

fn start_local_proxies(
    config: &SidecarConfig,
    tunnel_tx: mpsc::Sender<egress_proxy::TunnelRequest>,
) -> Result<(bool, Vec<JoinHandle<()>>)> {
    let cleanup = if config.enable_egress && !config.skip_egress_nft {
        ensure!(
            egress_nft::has_net_admin_capability(),
            "transparent egress requires CAP_NET_ADMIN"
        );
        egress_nft::setup_egress_rules(&egress_nft::EgressNftConfig::default())?;
        true
    } else {
        false
    };
    let mut tasks = Vec::new();
    if config.enable_egress {
        let proxy = egress_proxy::EgressProxy::new(
            egress_proxy::EgressProxyConfig::default(),
            tunnel_tx.clone(),
        );
        tasks.push(tokio::spawn(async move {
            if let Err(error) = proxy.run().await {
                log::error!("transparent egress proxy failed: {error}");
            }
        }));
    }
    if let Some(port) = config.http_proxy_port {
        let proxy = http_connect_proxy::HttpConnectProxy::new(
            http_connect_proxy::HttpConnectProxyConfig {
                listen_port: if port == 0 {
                    http_connect_proxy::HTTP_CONNECT_PROXY_PORT
                } else {
                    port
                },
                listen_host: "127.0.0.1".into(),
            },
            tunnel_tx,
        );
        tasks.push(tokio::spawn(async move {
            if let Err(error) = proxy.run().await {
                log::error!("HTTP CONNECT proxy failed: {error}");
            }
        }));
    }
    Ok((cleanup, tasks))
}

fn endpoint_id(record: &EndpointRecord) -> Result<EndpointId> {
    Ok(iroh_support::endpoint_addr(record, now_secs())?.id)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn notify(sender: &Option<mpsc::UnboundedSender<SidecarEvent>>, event: SidecarEvent) {
    if let Some(sender) = sender {
        let _ = sender.send(event);
    }
}
