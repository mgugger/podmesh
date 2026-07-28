mod handlers;

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, ensure};
use iroh::{
    Endpoint, EndpointId, RelayMode,
    endpoint::{Connection, presets},
};
use log::{debug, info, warn};
use protocol::{
    DEFAULT_WORKLOAD_STREAM_TIMEOUT, ENDPOINT_RECORD_VERSION, EndpointRecord, ProxyHttpRequest,
    ProxyHttpResponse, SidecarRoute, WORKLOAD_ALPN, WorkloadStreamKind, read_workload_frame,
    write_workload_frame,
};
use tokio::{
    sync::{RwLock as AsyncRwLock, Semaphore, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{config::Config, relay, restapi::ProxyGrantStore};

pub use handlers::evaluate_sidecar_registration;

const MAX_WORKLOAD_CONNECTIONS: usize = 4_096;
const MAX_CONCURRENT_WORKLOAD_STREAMS: usize = 1_024;
const MAX_CONCURRENT_INGRESS_STREAMS: usize = 256;
const ROUTE_PRUNE_INTERVAL: Duration = Duration::from_secs(5);
const SIDECAR_REGISTRATION_TTL: Duration = Duration::from_secs(120);
const ENDPOINT_RECORD_LIFETIME: Duration = Duration::from_secs(60 * 60);
const ENDPOINT_RECORD_REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
pub struct SidecarRouteEntry {
    pub sidecar_peer_id: String,
    pub routes: Vec<SidecarRoute>,
    pub registered_at: u64,
}

pub type RoutingTable = Arc<RwLock<HashMap<String, SidecarRouteEntry>>>;

pub(crate) struct RuntimeState {
    pub endpoint: Endpoint,
    pub connections: AsyncRwLock<HashMap<EndpointId, Connection>>,
    pub routing_table: RoutingTable,
    pub grant_store: ProxyGrantStore,
    pub own_endpoint_record: Arc<RwLock<EndpointRecord>>,
    pub known_proxies: AsyncRwLock<HashMap<EndpointId, EndpointRecord>>,
    pub peer_tx: watch::Sender<Vec<String>>,
    pub cancellation: CancellationToken,
    pub stream_slots: Arc<Semaphore>,
    pub ingress_slots: Arc<Semaphore>,
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
    pub async fn forward(&self, mut request: ProxyHttpRequest) -> Result<ProxyHttpResponse> {
        let _permit = tokio::time::timeout(
            DEFAULT_WORKLOAD_STREAM_TIMEOUT,
            self.state.ingress_slots.clone().acquire_owned(),
        )
        .await
        .context("timed out waiting for ingress stream capacity")?
        .context("ingress stream limiter closed")?;
        let route = self
            .state
            .routing_table
            .read()
            .map_err(|_| anyhow!("routing table lock poisoned"))?
            .get(&request.manifest_id)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "manifest {} has no registered sidecar route",
                    request.manifest_id
                )
            })?;
        let host = extract_host_header(&request.headers);
        request.target_port =
            select_route_port(&route.routes, &request.path_and_query, host.as_deref())
                .or_else(|| (request.target_port != 0).then_some(request.target_port))
                .ok_or_else(|| anyhow!("no matching route for ingress request"))?;
        let endpoint_id = parse_endpoint_id(&route.sidecar_peer_id)?;
        let connection = self
            .state
            .connections
            .read()
            .await
            .get(&endpoint_id)
            .cloned()
            .ok_or_else(|| anyhow!("registered sidecar connection is unavailable"))?;
        let (mut send, mut recv) =
            tokio::time::timeout(DEFAULT_WORKLOAD_STREAM_TIMEOUT, connection.open_bi())
                .await
                .context("timed out opening ingress stream")?
                .context("open ingress stream")?;
        let payload = postcard::to_allocvec(&request).context("serialize ingress request")?;
        write_workload_frame(
            &mut send,
            WorkloadStreamKind::Ingress,
            &payload,
            DEFAULT_WORKLOAD_STREAM_TIMEOUT,
            &self.state.cancellation,
        )
        .await?;
        send.finish().context("finish ingress request")?;
        let (kind, payload) = read_workload_frame(
            &mut recv,
            DEFAULT_WORKLOAD_STREAM_TIMEOUT,
            &self.state.cancellation,
        )
        .await?;
        ensure!(
            kind == WorkloadStreamKind::Ingress,
            "unexpected ingress response kind"
        );
        postcard::from_bytes(&payload).context("decode ingress response")
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

    pub fn routing_table(&self) -> RoutingTable {
        self.state.routing_table.clone()
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
    config.validate()?;
    let relay_server = match &config.workload_relay {
        Some(relay_config) => Some(relay::start(relay_config).await?),
        None => None,
    };
    let secret = config.identity.load()?;
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
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
    let endpoint_record = Arc::new(RwLock::new(signed_endpoint_record(&endpoint).await?));
    let endpoint_id = endpoint.id().to_string();
    let (peer_tx, peer_rx) = watch::channel(Vec::new());
    let (_network_ready_tx, network_ready_rx) = watch::channel(true);
    let cancellation = CancellationToken::new();
    let state = Arc::new(RuntimeState {
        endpoint: endpoint.clone(),
        connections: AsyncRwLock::new(HashMap::new()),
        routing_table: Arc::new(RwLock::new(HashMap::new())),
        grant_store: ProxyGrantStore::new(),
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
    });
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
            _ = prune.tick() => prune_stale_routes(&state.routing_table),
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
    let payload = record.to_bytes(now_secs()?)?;
    write_workload_frame(
        &mut send,
        WorkloadStreamKind::ProxyAnnouncement,
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
        kind == WorkloadStreamKind::ProxyAnnouncement,
        "proxy announcement response kind is invalid"
    );
    let remote_record = EndpointRecord::from_bytes(&response, now_secs()?)?;
    ensure!(
        remote_record.endpoint_id.as_slice() == connection.remote_id().as_bytes(),
        "proxy announcement response does not match transport"
    );
    state
        .known_proxies
        .write()
        .await
        .insert(connection.remote_id(), remote_record);
    Ok(())
}

async fn refresh_endpoint_record(state: &RuntimeState) -> Result<()> {
    let refreshed = signed_endpoint_record(&state.endpoint).await?;
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
        if let Some(connection) = connections.get(&proxy_id).cloned() {
            if let Err(error) = announce_proxy(state, &connection).await {
                warn!(
                    "failed to refresh proxy announcement endpoint={} error={error}",
                    proxy_id.fmt_short()
                );
            }
        }
    }
    Ok(())
}

async fn register_connection(state: Arc<RuntimeState>, connection: Connection) {
    let remote = connection.remote_id();
    {
        let mut connections = state.connections.write().await;
        if !connections.contains_key(&remote) && connections.len() >= MAX_WORKLOAD_CONNECTIONS {
            connection.close(1u8.into(), b"connection limit reached");
            return;
        }
        if let Some(previous) = connections.insert(remote, connection.clone()) {
            previous.close(2u8.into(), b"connection replaced");
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
                    tokio::spawn(async move {
                        if let Err(error) = handlers::handle_stream(state, remote, send, recv).await {
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
        .is_some_and(|current| current.stable_id() == connection.stable_id())
    {
        connections.remove(&remote);
        publish_peers(&connections, &state.peer_tx);
    }
}

fn publish_peers(
    connections: &HashMap<EndpointId, Connection>,
    sender: &watch::Sender<Vec<String>>,
) {
    let mut peers = connections
        .keys()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    peers.sort_unstable();
    let _ = sender.send(peers);
}

fn prune_stale_routes(table: &RoutingTable) {
    let now = now_millis();
    if let Ok(mut routes) = table.write() {
        routes.retain(|_, entry| {
            now.saturating_sub(entry.registered_at)
                <= u64::try_from(SIDECAR_REGISTRATION_TTL.as_millis()).unwrap_or(u64::MAX)
        });
    }
}

async fn signed_endpoint_record(endpoint: &Endpoint) -> Result<EndpointRecord> {
    let now = now_secs()?;
    let expires = now.saturating_add(ENDPOINT_RECORD_LIFETIME.as_secs());
    let address = endpoint.addr();
    let (signing_public, signing_private) = crypto::ensure_keypair_on_disk()?;
    EndpointRecord {
        version: ENDPOINT_RECORD_VERSION,
        endpoint_id: endpoint.id().as_bytes().to_vec(),
        relay_url: address.relay_urls().next().map(ToString::to_string),
        direct_addresses: address.ip_addrs().map(ToString::to_string).collect(),
        signing_pubkey: String::new(),
        issued_at_secs: now,
        expires_at_secs: expires,
        signature: String::new(),
    }
    .sign(&signing_public, &signing_private, now)
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

fn select_route_port(routes: &[SidecarRoute], path: &str, _host: Option<&str>) -> Option<u16> {
    let normalized = path.split('?').next().unwrap_or(path);
    routes
        .iter()
        .filter(|route| normalized.starts_with(&route.path_prefix))
        .max_by_key(|route| route.path_prefix.len())
        .map(|route| route.port)
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
