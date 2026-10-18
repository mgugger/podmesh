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

pub async fn run_with_metrics(
    config: SidecarConfig,
    mut shutdown: oneshot::Receiver<()>,
    event_tx: Option<mpsc::UnboundedSender<SidecarEvent>>,
    metrics: podmesh_metrics::Metrics,
) -> Result<()> {
    config.validate()?;
    metrics.set_gauge(podmesh_metrics::GaugeName::ProxySessions, 0);
    metrics.set_gauge(podmesh_metrics::GaugeName::ActiveIngress, 0);
    metrics.set_gauge(podmesh_metrics::GaugeName::ActiveEgress, 0);
    metrics.set_gauge(podmesh_metrics::GaugeName::ActiveWebsockets, 0);
    let replay_registry = Arc::new(protocol::PeerReplayRegistry::new(
        config.workload_replay_limits,
    )?);
    let identity = Arc::new(config.identity.load()?);
    let owner = config
        .owner_public_key_b64
        .as_deref()
        .context("workload owner missing")?;
    let workload_id = protocol::workload_id(
        &crypto::b64_decode(owner)?,
        &config.workload_name,
        config.replica_index,
    );
    let authorization = Arc::new(crate::authorization::WorkloadAuthorization::new(
        config
            .workload_credential_b64
            .as_deref()
            .context("workload enrollment authority missing")?,
        owner,
        &config.manifest_id,
        &workload_id,
        identity.endpoint_id().to_string(),
        identity.signing_public().to_vec(),
    )?);
    let mut authority_changes = authorization.subscribe();
    let endpoint = bind_endpoint(&config, &identity).await?;
    let connector_control = match &config.connector_socket {
        Some(path) => Some(crate::connector_control::ConnectorControl::bind(path).await?),
        None => None,
    };
    let cancellation = CancellationToken::new();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let network = if config.service_catalog.is_some() {
        let directory = match &config.identity {
            crate::IdentitySource::Persistent(path) => Some(path.clone()),
            _ => None,
        };
        Some(crate::workload_network::WorkloadNetwork::new(
            owner.to_string(),
            config.service_catalog.clone(),
            config.replica_index,
            endpoint.clone(),
            identity.clone(),
            authorization.clone(),
            directory,
            cancellation.clone(),
        )?)
    } else {
        None
    };
    let dns_servers = match &network {
        Some(network) => {
            crate::workload_network::bind_dns(network, !config.skip_egress_nft).await?
        }
        None => Vec::new(),
    };
    let mut network_task = network.as_ref().map(|network| {
        tokio_util::task::AbortOnDropHandle::new(tokio::spawn(network.clone().run()))
    });
    let http_client = Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .context("build sidecar HTTP client")?;
    let stream_slots = Arc::new(Semaphore::new(MAX_CONCURRENT_INCOMING_STREAMS));
    let stream_metrics = streams::StreamMetrics::new(metrics.clone());
    let connect_slots = Arc::new(Semaphore::new(MAX_PARALLEL_CONNECTION_ATTEMPTS));
    let (candidate_tx, mut candidate_rx) = mpsc::channel::<EndpointRecord>(MAX_PROXY_CANDIDATES);
    let (connected_tx, mut connected_rx) = mpsc::channel(MAX_PROXY_CANDIDATES);
    let (disconnected_tx, mut disconnected_rx) = mpsc::channel(MAX_PROXY_CANDIDATES);
    let (tunnel_tx, mut tunnel_rx) = mpsc::channel(256);
    let (egress_cleanup, listener_tasks) =
        start_local_proxies(&config, tunnel_tx, network.is_some(), dns_servers.len() > 1)?;

    let mut candidates = HashMap::<EndpointId, EndpointRecord>::new();
    let mut connecting = HashSet::<EndpointId>::new();
    let mut sessions = HashMap::<EndpointId, ProxySession>::new();
    for record in config.proxy_endpoints.clone() {
        candidate_tx
            .try_send(record)
            .map_err(|_| anyhow::anyhow!("initial proxy candidate queue is full"))?;
    }
    let mut connector_task = if let Some(control) = connector_control {
        let context = crate::connector_control::ConnectorContext {
            owner: config
                .owner_public_key_b64
                .clone()
                .context("connector owner missing")?,
            workload_name: config.workload_name.clone(),
            replica_index: config.replica_index,
            replica_count: config.replica_count,
            endpoint: endpoint.clone(),
            identity: identity.clone(),
            authorization: authorization.clone(),
            network: network.clone(),
            require_relay_hint: config
                .proxy_endpoints
                .iter()
                .any(|record| record.relay_url.is_some()),
        };
        Some(tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
            control.serve(context, cancellation.clone()),
        )))
    } else {
        None
    };
    let config = Arc::new(config);
    let mut connector_result = Ok(());
    let mut reconnect = tokio::time::interval(PROXY_RECONNECT_INTERVAL);
    let mut registration = tokio::time::interval(REGISTRATION_REFRESH_INTERVAL);
    let mut discovery = tokio::time::interval(config.lookup_interval);

    let runtime_result = async {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            result = async {
                match network_task.as_mut() { Some(task) => task.await.context("workload network task failed")?, None => std::future::pending::<Result<()>>().await }
            } => {
                network_task = None;
                result?;
                return Err(anyhow::anyhow!("workload networking stopped unexpectedly"));
            },
            result = authority_changes.changed() => {
                result.context("workload authority channel closed")?;
                for session in sessions.values_mut() {
                    match connection::refresh_authorization(&identity, &config, session, &authorization, &cancellation).await {
                        Ok(grant) => session.proxy_grant = grant,
                        Err(error) => {
                            log::warn!("workload session authority refresh failed: {error}");
                            session.connection.close(2u8.into(), b"authority refresh failed");
                        }
                    }
                }
                // Enrollment should wake initial connections immediately rather
                // than wait for the periodic reconnect timer.
                for (id, record) in candidates.clone() {
                    if !sessions.contains_key(&id) && connecting.insert(id) {
                        spawn_connection_attempt(endpoint.clone(), identity.clone(), config.clone(), record,
                            cancellation.clone(), connect_slots.clone(), replay_registry.clone(),
                            authorization.clone(), metrics.clone(), connected_tx.clone());
                    }
                }
            },
            result = async {
                match connector_task.as_mut() {
                    Some(task) => task.await.context("connector control task failed")?,
                    None => std::future::pending::<Result<()>>().await,
                }
            } => {
                connector_task = None;
                connector_result = result.and_then(|()| Err(anyhow::anyhow!("connector control stopped unexpectedly")));
                break;
            },
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
                        identity.clone(),
                        config.clone(),
                        record,
                        cancellation.clone(),
                        connect_slots.clone(),
                        replay_registry.clone(),
                        authorization.clone(),
                        metrics.clone(),
                        connected_tx.clone(),
                    );
                }
            }
            Some((record, result)) = connected_rx.recv() => {
                let id = endpoint_id(&record)?;
                connecting.remove(&id);
                match result {
                    Ok(mut session) => {
                        // A credential can renew while the initial handshake is
                        // in flight. Install the latest authority before routing.
                        match connection::refresh_authorization(&identity, &config, &session, &authorization, &cancellation).await {
                            Ok(grant) => session.proxy_grant = grant,
                            Err(error) => {
                                session.connection.close(2u8.into(), b"authority verification failed");
                                log::warn!("connected workload authority is no longer valid: {error}");
                                continue;
                            }
                        }
                        if let Some(previous) = sessions.insert(id, session.clone()) {
                            previous.connection.close(2u8.into(), b"proxy session replaced");
                        }
                        notify(&event_tx, SidecarEvent::Connected { peer_id: id.to_string() });
                        notify(&event_tx, SidecarEvent::ProxyPeerDiscovered { peer_id: id.to_string() });
                        metrics.set_gauge(
                            podmesh_metrics::GaugeName::ProxySessions,
                            sessions.len() as u64,
                        );
                        if let Err(error) = connection::register(endpoint.id(), &config, &session, &cancellation).await {
                            log::warn!("initial sidecar registration failed endpoint={} error={error}", id.fmt_short());
                        }
                        // Only a verified session may carry tenant traffic: an
                        // unverified proxy has not proved the owner authorised
                        // it, so it must not be able to reach the application.
                        tokio::spawn(streams::serve_connection(
                            session.connection.clone(),
                            session.stream_slots.clone(),
                            session.identity.clone(),
                            session.replay_registry.clone(),
                            session.proxy_grant.clone(),
                            stream_metrics.clone(),
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
                metrics.set_gauge(
                    podmesh_metrics::GaugeName::ProxySessions,
                    sessions.len() as u64,
                );
            }
            _ = reconnect.tick() => {
                for (id, record) in candidates.clone() {
                    if !sessions.contains_key(&id) && connecting.insert(id) {
                        spawn_connection_attempt(
                            endpoint.clone(),
                            identity.clone(),
                            config.clone(),
                            record,
                            cancellation.clone(),
                            connect_slots.clone(),
                            replay_registry.clone(),
                            authorization.clone(),
                            metrics.clone(),
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
                if let Some(network) = &network && network.is_internal_destination(&tunnel.dest_host) {
                    let network = network.clone();
                    tokio::spawn(async move { if let Err(error) = network.forward(tunnel).await { log::debug!("internal service connection failed: {error}"); } });
                    continue;
                }
                if let Some(session) = sessions.values().find(|session| session.verified) {
                    tokio::spawn(streams::open_egress(
                        session.clone(),
                        tunnel,
                        cancellation.clone(),
                        stream_metrics.clone(),
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

    Ok::<(), anyhow::Error>(())
    }.await;

    cancellation.cancel();
    if let Some(task) = connector_task {
        connector_result = match task.await {
            Ok(result) => result,
            Err(error) => {
                Err(anyhow::Error::new(error).context("connector control shutdown failed"))
            }
        };
    }
    if let Some(task) = network_task {
        let _ = task.await;
    }
    for dns in dns_servers {
        if let Err(error) = dns.shutdown().await {
            log::warn!("DNS shutdown failed: {error}");
        }
    }
    if let Some(network) = &network
        && let Err(error) = network.shutdown().await
    {
        log::warn!("peer shutdown failed: {error}");
    }
    endpoint.close().await;
    for task in listener_tasks {
        task.abort();
        let _ = task.await;
    }
    if egress_cleanup && let Err(error) = egress_nft::cleanup_egress_rules() {
        log::warn!("failed to clean up sidecar egress rules: {error}");
    }
    runtime_result.and(connector_result)
}

async fn bind_endpoint(
    config: &SidecarConfig,
    identity: &iroh_support::NodeIdentity,
) -> Result<Endpoint> {
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(identity.transport_secret().clone())
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

#[allow(clippy::too_many_arguments)]
fn spawn_connection_attempt(
    endpoint: Endpoint,
    identity: Arc<iroh_support::NodeIdentity>,
    config: Arc<SidecarConfig>,
    record: EndpointRecord,
    cancellation: CancellationToken,
    slots: Arc<Semaphore>,
    replay_registry: Arc<protocol::PeerReplayRegistry>,
    authorization: Arc<crate::authorization::WorkloadAuthorization>,
    metrics: podmesh_metrics::Metrics,
    sender: mpsc::Sender<(EndpointRecord, Result<ProxySession>)>,
) {
    tokio::spawn(async move {
        let timer = metrics.operation_started(podmesh_metrics::OperationName::ProxyConnection);
        let result = match slots.acquire_owned().await {
            Ok(_permit) => {
                connection::connect(
                    &endpoint,
                    &identity,
                    &config,
                    record.clone(),
                    replay_registry,
                    &authorization,
                    metrics.clone(),
                    &cancellation,
                )
                .await
            }
            Err(_) => Err(anyhow::anyhow!("proxy connection limiter closed")),
        };
        finish_operation(timer, &result, &metrics);
        let _ = sender.send((record, result)).await;
    });
}

fn finish_operation<T>(
    timer: podmesh_metrics::OperationTimer,
    result: &Result<T>,
    metrics: &podmesh_metrics::Metrics,
) {
    let (outcome, reason) = match result {
        Ok(_) => (
            podmesh_metrics::Outcome::Success,
            podmesh_metrics::Reason::None,
        ),
        Err(error) if error.to_string().contains("replay") => {
            metrics.record_event(podmesh_metrics::EventName::ReplayRefusal);
            (
                podmesh_metrics::Outcome::Refused,
                podmesh_metrics::Reason::Replay,
            )
        }
        Err(error) if error.to_string().contains("timed out") => (
            podmesh_metrics::Outcome::Timeout,
            podmesh_metrics::Reason::Deadline,
        ),
        Err(error) if error.to_string().contains("limit") => {
            metrics.record_event(podmesh_metrics::EventName::StreamSaturation);
            (
                podmesh_metrics::Outcome::Saturated,
                podmesh_metrics::Reason::Capacity,
            )
        }
        Err(error)
            if error.to_string().contains("grant") || error.to_string().contains("credential") =>
        {
            (
                podmesh_metrics::Outcome::Refused,
                podmesh_metrics::Reason::Authorization,
            )
        }
        Err(_) => (
            podmesh_metrics::Outcome::Unreachable,
            podmesh_metrics::Reason::Unavailable,
        ),
    };
    timer.finish(outcome, reason);
}

fn start_local_proxies(
    config: &SidecarConfig,
    tunnel_tx: mpsc::Sender<egress_proxy::TunnelRequest>,
    dns: bool,
    ipv6_dns: bool,
) -> Result<(bool, Vec<JoinHandle<()>>)> {
    let bound_proxy = if config.enable_egress || dns {
        let proxy = egress_proxy::EgressProxy::new(
            egress_proxy::EgressProxyConfig::default(),
            tunnel_tx.clone(),
        );
        let listener = proxy.bind()?;
        Some((proxy, listener))
    } else {
        None
    };
    let bound_http = if let Some(port) = config.http_proxy_port {
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
        let listener = proxy.bind()?;
        Some((proxy, listener))
    } else {
        None
    };
    let cleanup = if (config.enable_egress || dns) && !config.skip_egress_nft {
        ensure!(
            egress_nft::has_net_admin_capability(),
            "transparent egress requires CAP_NET_ADMIN"
        );
        let rules = egress_nft::EgressNftConfig {
            dns_port: dns.then_some(egress_nft::DNS_PORT),
            ipv6_dns,
            redirect_external: config.enable_egress,
            ..egress_nft::EgressNftConfig::default()
        };
        if let Err(error) = egress_nft::setup_egress_rules(&rules) {
            let _ = egress_nft::cleanup_egress_rules();
            return Err(error);
        }
        true
    } else {
        false
    };
    let mut tasks = Vec::new();
    if let Some((proxy, listener)) = bound_proxy {
        tasks.push(tokio::spawn(async move {
            if let Err(error) = proxy.run_bound(listener).await {
                log::error!("transparent egress proxy failed: {error}");
            }
        }));
    }
    if let Some((proxy, listener)) = bound_http {
        tasks.push(tokio::spawn(async move {
            if let Err(error) = proxy.run_bound(listener).await {
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
