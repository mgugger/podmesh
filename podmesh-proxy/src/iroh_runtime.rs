mod egress;
mod handlers;
mod tenant_gate;

use crate::routes::RouteTarget;
use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, ensure};
use axum::body::{Body, Bytes};
use futures::StreamExt;
use iroh::{
    Endpoint, EndpointId, RelayMode,
    endpoint::{Connection, presets},
};
use log::{debug, info, warn};
use protocol::{
    DEFAULT_WORKLOAD_STREAM_TIMEOUT, ENDPOINT_RECORD_VERSION, EndpointRecord,
    IngressRequestMetadata, IngressResponseMetadata, WORKLOAD_ALPN, WorkloadEnvelopeParts,
    WorkloadPayload, accept_workload_payload, finish_http_body, read_http_body_chunk,
    read_workload_frame, seal_workload_payload, write_http_body_chunk, write_workload_frame,
};
use tokio::{
    io::split,
    sync::{RwLock as AsyncRwLock, Semaphore, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{config::Config, relay, restapi::ProxyGrantStore};

pub use handlers::MAX_REGISTERED_SIDECARS;

const MAX_WORKLOAD_CONNECTIONS: usize = 4_096;
const MAX_CONCURRENT_WORKLOAD_STREAMS: usize = 1_024;
const MAX_CONCURRENT_INGRESS_STREAMS: usize = 256;
const ROUTE_PRUNE_INTERVAL: Duration = Duration::from_secs(5);
const SIDECAR_REGISTRATION_TTL: Duration = Duration::from_secs(120);
const ENDPOINT_RECORD_LIFETIME: Duration = Duration::from_secs(60 * 60);
const ENDPOINT_RECORD_REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);

pub(crate) struct RuntimeState {
    pub endpoint: Endpoint,
    /// This proxy's own keys, used to sign handshakes and its endpoint record.
    pub identity: iroh_support::NodeIdentity,
    pub replay_registry: Arc<protocol::PeerReplayRegistry>,
    pub connections: AsyncRwLock<HashMap<EndpointId, WorkloadConnection>>,
    pub routes: Arc<crate::routes::RouteTable>,
    pub grant_store: ProxyGrantStore,
    /// Tenancy each open connection proved, never what it claimed.
    pub tenants: crate::tenant_sessions::TenantSessions,
    /// Addresses published in place of the ones the endpoint bound.
    pub advertise_addresses: Vec<String>,
    pub own_endpoint_record: Arc<RwLock<EndpointRecord>>,
    pub known_proxies: AsyncRwLock<HashMap<EndpointId, EndpointRecord>>,
    pub peer_tx: watch::Sender<Vec<String>>,
    pub cancellation: CancellationToken,
    pub stream_slots: Arc<Semaphore>,
    pub ingress_slots: Arc<Semaphore>,
    pub metrics: podmesh_metrics::Metrics,
    pub active_ingress: Arc<AtomicUsize>,
    pub active_egress: Arc<AtomicUsize>,
    pub active_websockets: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub(crate) struct WorkloadConnection {
    connection: Connection,
    stream_slots: Arc<Semaphore>,
}

pub struct IrohNodeHandle {
    task: JoinHandle<()>,
    endpoint: Endpoint,
    relay_server: Option<iroh_relay::server::Server>,
    cancellation: CancellationToken,
    peer_rx: watch::Receiver<Vec<String>>,
    endpoint_id: String,
    endpoint_record: Arc<RwLock<EndpointRecord>>,
    network_ready_rx: watch::Receiver<bool>,
    state: Arc<RuntimeState>,
}

#[derive(Clone)]
pub struct ProxyClient {
    state: Arc<RuntimeState>,
}

impl ProxyClient {
    pub async fn forward(
        &self,
        request: IngressRequestMetadata,
        body: Body,
        on_upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> Result<axum::http::Response<Body>> {
        let timer = self
            .state
            .metrics
            .operation_started(podmesh_metrics::OperationName::Ingress);
        let result = self.forward_inner(request, body, on_upgrade).await;
        finish_operation(timer, &result, &self.state.metrics);
        result
    }

    async fn forward_inner(
        &self,
        request: IngressRequestMetadata,
        body: Body,
        on_upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> Result<axum::http::Response<Body>> {
        let permit = tokio::time::timeout(
            DEFAULT_WORKLOAD_STREAM_TIMEOUT,
            self.state.ingress_slots.clone().acquire_owned(),
        )
        .await;
        let permit = match permit {
            Ok(Ok(permit)) => permit,
            Ok(Err(error)) => {
                self.state
                    .metrics
                    .record_event(podmesh_metrics::EventName::StreamSaturation);
                return Err(error).context("ingress stream limiter closed");
            }
            Err(error) => {
                self.state
                    .metrics
                    .record_event(podmesh_metrics::EventName::StreamSaturation);
                return Err(error).context("timed out waiting for ingress stream capacity");
            }
        };
        let _active = ActiveGauge::new(
            self.state.metrics.clone(),
            self.state.active_ingress.clone(),
            podmesh_metrics::GaugeName::ActiveIngress,
        );
        let _permit = permit;
        let host = extract_host_header(&request.headers);
        // Every replica of the deployment is a candidate, rotated so
        // consecutive requests spread across them.
        let targets = self.state.routes.select(
            &request.manifest_id,
            &request.path_and_query,
            host.as_deref(),
        );
        ensure!(
            !targets.is_empty(),
            "manifest {} has no registered sidecar route",
            request.manifest_id
        );

        // Fall through to the next replica when one cannot be reached, so a
        // replica that died between its last registration refresh and now does
        // not blackhole the request.
        let mut last_error = None;
        let mut selected = None;
        for target in &targets {
            let mut attempt = request.clone();
            attempt.target_port = target.port;
            match self.open_ingress(&attempt, target).await {
                Ok((streams, stable_id)) => {
                    selected = Some((
                        attempt,
                        parse_endpoint_id(&target.sidecar_peer_id)?,
                        stable_id,
                        streams,
                    ));
                    break;
                }
                Err(error) => {
                    log::warn!(
                        "ingress to replica {} of manifest {} failed: {error:#}",
                        target.replica_index,
                        request.manifest_id
                    );
                    last_error = Some(error);
                }
            }
        }
        let (request, selected_endpoint, stable_id, (mut send, mut recv)) =
            selected.ok_or_else(|| {
                last_error.unwrap_or_else(|| {
                    anyhow!("no replica could serve manifest {}", request.manifest_id)
                })
            })?;
        let mut body_progress = protocol::workload_body::HttpBodyProgress::default();
        let mut body = body.into_data_stream();
        while let Some(chunk) = body.next().await {
            write_http_body_chunk(
                &mut send,
                &chunk.context("read external ingress request body")?,
                &mut body_progress,
                DEFAULT_WORKLOAD_STREAM_TIMEOUT,
                &self.state.cancellation,
            )
            .await?;
        }
        finish_http_body(
            &mut send,
            DEFAULT_WORKLOAD_STREAM_TIMEOUT,
            &self.state.cancellation,
        )
        .await?;
        if !request.upgrade_requested {
            send.finish().context("finish ingress request stream")?;
        }
        let (kind, response) = read_workload_frame(
            &mut recv,
            DEFAULT_WORKLOAD_STREAM_TIMEOUT,
            &self.state.cancellation,
        )
        .await?;
        ensure!(
            kind == <IngressResponseMetadata as WorkloadPayload>::TYPE.frame_kind(),
            "unexpected ingress response kind"
        );
        let accepted = accept_workload_payload::<IngressResponseMetadata>(
            &response,
            &self.state.endpoint.id().to_string(),
            &selected_endpoint.to_string(),
            now_millis(),
            &self.state.replay_registry,
            Instant::now(),
        )?;
        let metadata = accepted.payload;
        if metadata.upgrade_accepted {
            ensure!(
                request.upgrade_requested && metadata.status_code == 101,
                "invalid ingress upgrade transition"
            );
            let on_upgrade = on_upgrade.context("external HTTP upgrade handle is missing")?;
            let state = self.state.clone();
            tokio::spawn(async move {
                let _active = ActiveGauge::new(
                    state.metrics.clone(),
                    state.active_websockets.clone(),
                    podmesh_metrics::GaugeName::ActiveWebsockets,
                );
                let result = async {
                    let upgraded = on_upgrade.await.context("complete external HTTP upgrade")?;
                    let upgraded = hyper_util::rt::TokioIo::new(upgraded);
                    let (external_read, external_write) = split(upgraded);
                    iroh_support::raw_relay::supervise_raw_relay(
                        external_read,
                        external_write,
                        recv,
                        send,
                        websocket_relay_limits(),
                        state.cancellation.clone(),
                        || tenant_gate::authorize_egress(&state, selected_endpoint, stable_id),
                    )
                    .await
                }
                .await;
                if let Err(error) = result {
                    warn!("WebSocket relay ended: {error:#}");
                }
            });
            let mut response = axum::http::Response::builder().status(metadata.status_code);
            for (name, value) in metadata.headers {
                response = response.header(name, value);
            }
            return response
                .body(Body::empty())
                .context("build WebSocket upgrade response");
        }
        let cancellation = self.state.cancellation.clone();
        let response_stream = futures::stream::unfold(
            Some((recv, protocol::workload_body::HttpBodyProgress::default())),
            move |state| {
                let cancellation = cancellation.clone();
                async move {
                    let (mut recv, mut progress) = state?;
                    match read_http_body_chunk(
                        &mut recv,
                        &mut progress,
                        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
                        &cancellation,
                    )
                    .await
                    {
                        Ok(Some(chunk)) => Some((
                            Ok::<Bytes, anyhow::Error>(chunk.into()),
                            Some((recv, progress)),
                        )),
                        Ok(None) => None,
                        Err(error) => Some((Err(error), None)),
                    }
                }
            },
        );
        let mut response = axum::http::Response::builder().status(metadata.status_code);
        for (name, value) in metadata.headers {
            response = response.header(name, value);
        }
        response
            .body(Body::from_stream(response_stream))
            .context("build streamed ingress response")
    }

    /// Send one already-routed request to one replica.
    async fn open_ingress(
        &self,
        request: &IngressRequestMetadata,
        target: &RouteTarget,
    ) -> Result<(
        (iroh::endpoint::SendStream, iroh::endpoint::RecvStream),
        usize,
    )> {
        let endpoint_id = parse_endpoint_id(&target.sidecar_peer_id)?;
        let connection = self
            .state
            .connections
            .read()
            .await
            .get(&endpoint_id)
            .cloned()
            .ok_or_else(|| anyhow!("registered sidecar connection is unavailable"))?;
        let _connection_permit = connection
            .stream_slots
            .clone()
            .try_acquire_owned()
            .context("sidecar connection stream limit reached")?;
        let (mut send, recv) = tokio::time::timeout(
            DEFAULT_WORKLOAD_STREAM_TIMEOUT,
            connection.connection.open_bi(),
        )
        .await
        .context("timed out opening ingress stream")?
        .context("open ingress stream")?;
        let payload = seal_workload_payload(
            WorkloadEnvelopeParts {
                nonce: &crypto::generate_secure_nonce(),
                now_millis: now_millis(),
                sender_id: &self.state.endpoint.id().to_string(),
                recipient_id: &endpoint_id.to_string(),
                sender_signing_public: self.state.identity.signing_public(),
                sender_signing_private: self.state.identity.signing_private(),
                sender_kem_public: Some(self.state.identity.kem_public()),
            },
            request,
        )?;
        write_workload_frame(
            &mut send,
            <IngressRequestMetadata as WorkloadPayload>::TYPE.frame_kind(),
            &payload,
            DEFAULT_WORKLOAD_STREAM_TIMEOUT,
            &self.state.cancellation,
        )
        .await?;
        Ok(((send, recv), connection.connection.stable_id()))
    }
}

pub(crate) struct ActiveGauge {
    metrics: podmesh_metrics::Metrics,
    active: Arc<AtomicUsize>,
    gauge: podmesh_metrics::GaugeName,
}

impl ActiveGauge {
    pub(crate) fn new(
        metrics: podmesh_metrics::Metrics,
        active: Arc<AtomicUsize>,
        gauge: podmesh_metrics::GaugeName,
    ) -> Self {
        let current = active.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        metrics.set_gauge(gauge, current as u64);
        Self {
            metrics,
            active,
            gauge,
        }
    }
}

impl Drop for ActiveGauge {
    fn drop(&mut self) {
        let previous = self.active.fetch_sub(1, Ordering::AcqRel);
        self.metrics
            .set_gauge(self.gauge, previous.saturating_sub(1) as u64);
    }
}

pub(crate) fn finish_operation<T>(
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
        Err(error)
            if error.to_string().contains("limit") || error.to_string().contains("capacity") =>
        {
            (
                podmesh_metrics::Outcome::Saturated,
                podmesh_metrics::Reason::Capacity,
            )
        }
        Err(error)
            if error.to_string().contains("grant")
                || error.to_string().contains("tenant")
                || error.to_string().contains("credential") =>
        {
            (
                podmesh_metrics::Outcome::Refused,
                podmesh_metrics::Reason::Authorization,
            )
        }
        Err(error)
            if error.to_string().contains("unavailable")
                || error.to_string().contains("connect") =>
        {
            (
                podmesh_metrics::Outcome::Unreachable,
                podmesh_metrics::Reason::Unavailable,
            )
        }
        Err(_) => (
            podmesh_metrics::Outcome::Refused,
            podmesh_metrics::Reason::Invalid,
        ),
    };
    timer.finish(outcome, reason);
}

fn websocket_relay_limits() -> iroh_support::raw_relay::RawRelayLimits {
    iroh_support::raw_relay::RawRelayLimits {
        max_bytes_per_direction: 512 * 1024 * 1024,
        idle_timeout: Duration::from_secs(300),
        max_lifetime: Duration::from_secs(60 * 60),
        authority_interval: Duration::from_secs(30),
    }
}

impl IrohNodeHandle {
    pub fn peer_id(&self) -> &str {
        &self.endpoint_id
    }

    pub fn endpoint_record(&self) -> Result<EndpointRecord> {
        self.endpoint_record
            .read()
            .map(|record| record.clone())
            .map_err(|_| anyhow!("proxy EndpointRecord lock poisoned"))
    }

    pub fn endpoint_record_handle(&self) -> Arc<RwLock<EndpointRecord>> {
        self.endpoint_record.clone()
    }

    pub fn peer_rx(&self) -> watch::Receiver<Vec<String>> {
        self.peer_rx.clone()
    }

    pub fn network_ready_rx(&self) -> watch::Receiver<bool> {
        self.network_ready_rx.clone()
    }

    pub fn proxy_client(&self) -> ProxyClient {
        ProxyClient {
            state: self.state.clone(),
        }
    }

    pub fn grant_store(&self) -> ProxyGrantStore {
        self.state.grant_store.clone()
    }

    pub fn routes(&self) -> Arc<crate::routes::RouteTable> {
        self.state.routes.clone()
    }

    pub async fn shutdown(self) {
        self.cancellation.cancel();
        self.endpoint.close().await;
        let _ = tokio::time::timeout(DEFAULT_WORKLOAD_STREAM_TIMEOUT, self.task).await;
        if let Some(server) = self.relay_server
            && let Err(error) = server.shutdown().await
        {
            warn!("failed to stop workload relay: {error}");
        }
    }
}

pub async fn spawn(config: &Config) -> Result<IrohNodeHandle> {
    spawn_with_metrics(config, podmesh_metrics::Metrics::noop()).await
}

pub async fn spawn_with_metrics(
    config: &Config,
    metrics: podmesh_metrics::Metrics,
) -> Result<IrohNodeHandle> {
    config.validate()?;
    let relay_server = match &config.workload_relay {
        Some(relay_config) => Some(relay::start(relay_config).await?),
        None => None,
    };
    let identity = config.identity.load()?;
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(identity.transport_secret().clone())
        .alpns(vec![WORKLOAD_ALPN.to_vec()])
        .bind_addr(config.iroh_bind_addr)?;
    if let Some(relay_config) = &config.workload_relay {
        builder = builder
            .relay_mode(RelayMode::Custom(relay_config.relay_map()?))
            .ca_tls_config(relay_config.ca_tls_config()?);
    } else {
        builder = builder.clear_relay_transports();
    }
    let endpoint = match builder.bind().await.context("bind proxy Iroh endpoint") {
        Ok(endpoint) => endpoint,
        Err(error) => {
            if let Some(server) = relay_server {
                let _ = server.shutdown().await;
            }
            return Err(error);
        }
    };
    let endpoint_record = Arc::new(RwLock::new(
        signed_endpoint_record(&endpoint, &identity, &config.advertise_addresses).await?,
    ));
    let endpoint_id = endpoint.id().to_string();
    let (peer_tx, peer_rx) = watch::channel(Vec::new());
    let (_network_ready_tx, network_ready_rx) = watch::channel(true);
    let cancellation = CancellationToken::new();
    let state = Arc::new(RuntimeState {
        endpoint: endpoint.clone(),
        identity,
        replay_registry: Arc::new(protocol::PeerReplayRegistry::new(
            config.workload_replay_limits,
        )?),
        connections: AsyncRwLock::new(HashMap::new()),
        routes: Arc::new(crate::routes::RouteTable::new().with_metrics(metrics.clone())),
        grant_store: ProxyGrantStore::new().with_metrics(metrics.clone()),
        tenants: crate::tenant_sessions::TenantSessions::new().with_metrics(metrics.clone()),
        advertise_addresses: config.advertise_addresses.clone(),
        own_endpoint_record: endpoint_record.clone(),
        known_proxies: AsyncRwLock::new(
            config
                .proxy_endpoints
                .iter()
                .filter_map(|record| {
                    parse_record_endpoint_id(record)
                        .ok()
                        .map(|id| (id, record.clone()))
                })
                .collect(),
        ),
        peer_tx,
        cancellation: cancellation.clone(),
        stream_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_WORKLOAD_STREAMS)),
        ingress_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_INGRESS_STREAMS)),
        metrics: metrics.clone(),
        active_ingress: Arc::new(AtomicUsize::new(0)),
        active_egress: Arc::new(AtomicUsize::new(0)),
        active_websockets: Arc::new(AtomicUsize::new(0)),
    });
    metrics.set_gauge(
        podmesh_metrics::GaugeName::ProxyPeers,
        state.known_proxies.read().await.len() as u64,
    );
    metrics.set_gauge(podmesh_metrics::GaugeName::ActiveIngress, 0);
    metrics.set_gauge(podmesh_metrics::GaugeName::ActiveEgress, 0);
    metrics.set_gauge(podmesh_metrics::GaugeName::ActiveWebsockets, 0);
    let task = tokio::spawn(run(state.clone()));
    info!("proxy Iroh endpoint ready endpoint_id={endpoint_id}");
    Ok(IrohNodeHandle {
        task,
        endpoint,
        relay_server,
        cancellation,
        peer_rx,
        endpoint_id,
        endpoint_record,
        network_ready_rx,
        state,
    })
}

async fn run(state: Arc<RuntimeState>) {
    let configured = state
        .known_proxies
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for record in configured {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = connect_configured_proxy(state, record).await {
                warn!("failed to connect configured proxy: {error}");
            }
        });
    }
    let mut prune = tokio::time::interval(ROUTE_PRUNE_INTERVAL);
    let mut endpoint_refresh = tokio::time::interval(ENDPOINT_RECORD_REFRESH_INTERVAL);
    endpoint_refresh.tick().await;
    loop {
        tokio::select! {
            _ = state.cancellation.cancelled() => break,
            _ = prune.tick() => state.routes.prune(now_millis(), SIDECAR_REGISTRATION_TTL),
            _ = endpoint_refresh.tick() => {
                if let Err(error) = refresh_endpoint_record(&state).await {
                    warn!("failed to refresh proxy EndpointRecord: {error}");
                }
            }
            incoming = state.endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let state = state.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(connection) => register_connection(state, connection).await,
                        Err(error) => warn!("failed to accept workload connection: {error}"),
                    }
                });
            }
        }
    }
}

async fn connect_configured_proxy(state: Arc<RuntimeState>, record: EndpointRecord) -> Result<()> {
    let address = iroh_support::endpoint_addr(&record, now_secs()?)?;
    let connection = tokio::time::timeout(
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        state.endpoint.connect(address, WORKLOAD_ALPN),
    )
    .await
    .context("configured proxy connection timed out")?
    .context("connect configured proxy")?;
    announce_proxy(&state, &connection).await?;
    register_connection(state, connection).await;
    Ok(())
}

async fn announce_proxy(state: &RuntimeState, connection: &Connection) -> Result<()> {
    let timer = state
        .metrics
        .operation_started(podmesh_metrics::OperationName::PeerAnnouncement);
    let result = announce_proxy_inner(state, connection).await;
    finish_operation(timer, &result, &state.metrics);
    result
}

async fn announce_proxy_inner(state: &RuntimeState, connection: &Connection) -> Result<()> {
    let (mut send, mut recv) =
        tokio::time::timeout(DEFAULT_WORKLOAD_STREAM_TIMEOUT, connection.open_bi())
            .await
            .context("proxy announcement stream timed out")?
            .context("open proxy announcement stream")?;
    let record = state
        .own_endpoint_record
        .read()
        .map_err(|_| anyhow!("proxy EndpointRecord lock poisoned"))?
        .clone();
    let request = protocol::ProxyAnnouncementRequest { endpoint: record };
    let payload = seal_workload_payload(
        WorkloadEnvelopeParts {
            nonce: &crypto::generate_secure_nonce(),
            now_millis: now_millis(),
            sender_id: &state.endpoint.id().to_string(),
            recipient_id: &connection.remote_id().to_string(),
            sender_signing_public: state.identity.signing_public(),
            sender_signing_private: state.identity.signing_private(),
            sender_kem_public: Some(state.identity.kem_public()),
        },
        &request,
    )?;
    write_workload_frame(
        &mut send,
        <protocol::ProxyAnnouncementRequest as WorkloadPayload>::TYPE.frame_kind(),
        &payload,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        &state.cancellation,
    )
    .await?;
    send.finish().context("finish proxy announcement")?;
    let (kind, response) = read_workload_frame(
        &mut recv,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        &state.cancellation,
    )
    .await?;
    ensure!(
        kind == <protocol::ProxyAnnouncementResponse as WorkloadPayload>::TYPE.frame_kind(),
        "proxy announcement response kind is invalid"
    );
    let accepted = accept_workload_payload::<protocol::ProxyAnnouncementResponse>(
        &response,
        &state.endpoint.id().to_string(),
        &connection.remote_id().to_string(),
        now_millis(),
        &state.replay_registry,
        Instant::now(),
    )?;
    let remote_record = accepted.payload.endpoint;
    ensure!(
        remote_record.endpoint_id.as_slice() == connection.remote_id().as_bytes(),
        "proxy announcement response does not match transport"
    );
    ensure!(
        accepted.sender_signing_key
            == crypto::b64_decode(&remote_record.signing_pubkey)
                .context("decode proxy announcement response signing key")?,
        "proxy announcement response envelope key does not match endpoint record"
    );
    state
        .known_proxies
        .write()
        .await
        .insert(connection.remote_id(), remote_record);
    state.metrics.set_gauge(
        podmesh_metrics::GaugeName::ProxyPeers,
        state.known_proxies.read().await.len() as u64,
    );
    Ok(())
}

async fn refresh_endpoint_record(state: &RuntimeState) -> Result<()> {
    let refreshed =
        signed_endpoint_record(&state.endpoint, &state.identity, &state.advertise_addresses)
            .await?;
    {
        let mut record = state
            .own_endpoint_record
            .write()
            .map_err(|_| anyhow!("proxy EndpointRecord lock poisoned"))?;
        *record = refreshed;
    }
    let proxy_ids = state
        .known_proxies
        .read()
        .await
        .keys()
        .copied()
        .collect::<Vec<_>>();
    let connections = state.connections.read().await;
    for proxy_id in proxy_ids {
        if let Some(connection) = connections.get(&proxy_id).cloned()
            && let Ok(_permit) = connection.stream_slots.clone().try_acquire_owned()
            && let Err(error) = announce_proxy(state, &connection.connection).await
        {
            warn!(
                "failed to refresh proxy announcement endpoint={} error={error}",
                proxy_id.fmt_short()
            );
        }
    }
    Ok(())
}

async fn register_connection(state: Arc<RuntimeState>, connection: Connection) {
    let remote = connection.remote_id();
    let stable_id = connection.stable_id();
    let connection_state = WorkloadConnection {
        connection: connection.clone(),
        stream_slots: Arc::new(Semaphore::new(
            protocol::MAX_WORKLOAD_STREAMS_PER_CONNECTION,
        )),
    };
    {
        let mut connections = state.connections.write().await;
        if !connections.contains_key(&remote) && connections.len() >= MAX_WORKLOAD_CONNECTIONS {
            state
                .metrics
                .record_event(podmesh_metrics::EventName::StreamSaturation);
            connection.close(1u8.into(), b"connection limit reached");
            return;
        }
        if connections.contains_key(&remote) {
            drop(connections);
            connection.close(2u8.into(), b"duplicate connection");
            return;
        }
        connections.insert(remote, connection_state.clone());
        if let Err(error) = state.tenants.begin(remote, stable_id) {
            connections.remove(&remote);
            drop(connections);
            connection.close(2u8.into(), b"duplicate connection session");
            warn!(
                "workload connection session refused endpoint={} error={error}",
                remote.fmt_short()
            );
            return;
        }
        publish_peers(&connections, &state.peer_tx);
    }
    info!(
        "workload connection established endpoint={}",
        remote.fmt_short()
    );
    loop {
        tokio::select! {
            _ = state.cancellation.cancelled() => break,
            _ = connection.closed() => break,
            stream = connection.accept_bi() => match stream {
                Ok((send, recv)) => {
                    let state = state.clone();
                    let connection_slots = connection_state.stream_slots.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handlers::handle_stream(
                            state,
                            remote,
                            stable_id,
                            connection_slots,
                            send,
                            recv,
                        ).await {
                            warn!("workload stream rejected endpoint={} error={error}", remote.fmt_short());
                        }
                    });
                }
                Err(error) => {
                    debug!("workload connection ended endpoint={} error={error}", remote.fmt_short());
                    break;
                }
            }
        }
    }
    let mut connections = state.connections.write().await;
    if connections
        .get(&remote)
        .is_some_and(|current| current.connection.stable_id() == connection.stable_id())
    {
        // Tenancy is proven per connection, so it disappears atomically with
        // the current connection rather than during stale-task cleanup.
        state.tenants.remove_if_current(&remote, stable_id);
        connections.remove(&remote);
        publish_peers(&connections, &state.peer_tx);
    }
}

fn publish_peers(
    connections: &HashMap<EndpointId, WorkloadConnection>,
    sender: &watch::Sender<Vec<String>>,
) {
    let mut peers = connections
        .keys()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    peers.sort_unstable();
    let _ = sender.send(peers);
}

/// The record sidecars and peer proxies dial this proxy with.
///
/// The addresses an endpoint discovers are the ones it bound, which in any
/// deployment where the proxy is not on the caller's network — behind NAT, in a
/// container, on another machine — are not reachable. `advertise_addresses`
/// replaces them for exactly that case.
async fn signed_endpoint_record(
    endpoint: &Endpoint,
    identity: &iroh_support::NodeIdentity,
    advertise_addresses: &[String],
) -> Result<EndpointRecord> {
    let now = now_secs()?;
    let expires = now.saturating_add(ENDPOINT_RECORD_LIFETIME.as_secs());
    let address = endpoint.addr();
    let direct_addresses: Vec<String> = if advertise_addresses.is_empty() {
        address.ip_addrs().map(ToString::to_string).collect()
    } else {
        advertise_addresses.to_vec()
    };
    let (signing_public, signing_private) = (identity.signing_public(), identity.signing_private());
    EndpointRecord {
        version: ENDPOINT_RECORD_VERSION,
        endpoint_id: endpoint.id().as_bytes().to_vec(),
        relay_url: address.relay_urls().next().map(ToString::to_string),
        direct_addresses,
        signing_pubkey: String::new(),
        issued_at_secs: now,
        expires_at_secs: expires,
        signature: String::new(),
    }
    .sign(signing_public, signing_private, now)
}

fn parse_endpoint_id(value: &str) -> Result<EndpointId> {
    value.parse().context("invalid Iroh EndpointId")
}

fn parse_record_endpoint_id(record: &EndpointRecord) -> Result<EndpointId> {
    let bytes: [u8; protocol::IROH_ENDPOINT_ID_BYTES] = record
        .endpoint_id
        .as_slice()
        .try_into()
        .context("proxy EndpointRecord ID length is invalid")?;
    EndpointId::from_bytes(&bytes).context("proxy EndpointRecord ID is invalid")
}

fn extract_host_header(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("host"))
        .map(|(_, value)| value.split(':').next().unwrap_or(value).to_lowercase())
}

pub(crate) fn now_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_secs())
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn sixty_fifth_connection_stream_is_refused_immediately() {
        let slots = Arc::new(Semaphore::new(
            protocol::MAX_WORKLOAD_STREAMS_PER_CONNECTION,
        ));
        let permits = (0..protocol::MAX_WORKLOAD_STREAMS_PER_CONNECTION)
            .map(|_| slots.clone().try_acquire_owned().unwrap())
            .collect::<Vec<_>>();
        assert!(slots.clone().try_acquire_owned().is_err());
        drop(permits);
        assert!(slots.try_acquire_owned().is_ok());
    }
}
