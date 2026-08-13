use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use axum_support::{parse_socket_addr, spawn_tcp_server, with_rate_limit};

use crate::relay_bootstrap_api::{get_workload_relay_bootstrap, get_workload_relay_mesh_secret};
use serde::Serialize;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

pub use crate::proxy_grants::ProxyGrantStore;

#[derive(Clone)]
pub struct PeerSnapshot {
    inner: Arc<Mutex<watch::Receiver<Vec<String>>>>,
}

impl PeerSnapshot {
    pub fn new(receiver: watch::Receiver<Vec<String>>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(receiver)),
        }
    }

    pub async fn latest(&self) -> Vec<String> {
        let guard = self.inner.lock().await;
        guard.borrow().clone()
    }
}

/// Everything a `podctl` client needs in order to place a workload behind this
/// proxy: the record used to dial it, plus the relay token and self-signed
/// relay certificate the injected sidecar must present and trust.
///
/// This is deliberately opt-in. The token grants use of the proxy's relay, so
/// publishing it is only appropriate on a trusted network such as a local
/// development mesh.
#[derive(Clone)]
pub struct WorkloadRelayBootstrap {
    /// Never served as-is. Each caller receives only the token derived for the
    /// tenant it asks about, so obtaining one tenant's token does not yield
    /// another's.
    pub mesh_secret: String,
    pub ca_certificate_der: Vec<u8>,
}

/// Inputs to the REST server.
/// Requests per minute a single peer address may make against the proxy REST
/// API.
///
/// `POST /api/v1/proxy_grant` is deliberately unauthenticated — a grant proves
/// its own authority — but verifying one costs a signature check plus a Datalog
/// evaluation, and every accepted grant occupies a slot in a bounded store. A
/// per-peer budget keeps that from being a free way to wedge the store. Real
/// clients post one grant per proxy per deploy, so the budget is generous.
pub const DEFAULT_REST_RATE_LIMIT_PER_MINUTE: u32 = 600;

pub struct RestServerOptions {
    pub host: String,
    pub port: u16,
    pub peer_rx: watch::Receiver<Vec<String>>,
    /// The proxy's Iroh EndpointId, which every submitted grant must name.
    pub local_peer_id: String,
    pub endpoint_record: Arc<RwLock<protocol::EndpointRecord>>,
    /// Owner-signed grants this proxy holds.
    pub grant_store: ProxyGrantStore,
    /// When set, `GET /api/v1/workload_relay_bootstrap` serves these values.
    pub relay_bootstrap: Option<WorkloadRelayBootstrap>,
    /// Per-peer request budget. Zero disables throttling.
    pub rate_limit_per_minute: u32,
}

#[derive(Clone)]
pub(crate) struct RestState {
    started_at: Instant,
    peers: PeerSnapshot,
    local_peer_id: String,
    pub(crate) endpoint_record: Arc<RwLock<protocol::EndpointRecord>>,
    grant_store: ProxyGrantStore,
    pub(crate) relay_bootstrap: Option<WorkloadRelayBootstrap>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    uptime_secs: u64,
    peer_count: usize,
}

#[derive(Serialize)]
struct PeerIdResponse {
    peer_id: String,
}

#[derive(Serialize)]
struct CertAck {
    ok: bool,
    owner_pubkey: String,
    message: String,
}

#[derive(Serialize)]
pub(crate) struct ApiError {
    pub(crate) error: String,
}

/// Response body for `GET /api/v1/endpoint_record`.
///
/// `peer_id` is a convenience for humans; consumers must take the endpoint id
/// from the signed record, because only that is attested.
#[derive(Serialize)]
struct EndpointRecordResponse {
    endpoint_record_b64: String,
    peer_id: String,
    signing_pubkey_b64: String,
}

pub fn spawn_rest_server(options: RestServerOptions) -> Result<JoinHandle<()>> {
    let RestServerOptions {
        host,
        port,
        peer_rx,
        local_peer_id,
        endpoint_record,
        grant_store,
        relay_bootstrap,
        rate_limit_per_minute,
    } = options;

    let addr = parse_socket_addr(&host, port)?;
    let state = RestState {
        started_at: Instant::now(),
        peers: PeerSnapshot::new(peer_rx),
        local_peer_id,
        endpoint_record,
        grant_store,
        relay_bootstrap,
    };

    let health_state = state.clone();
    let limited = Router::new()
        .route("/api/v1/peer_id", get(get_peer_id))
        .route("/api/v1/endpoint_record", get(get_endpoint_record))
        .route(
            "/api/v1/workload_relay_bootstrap",
            get(get_workload_relay_bootstrap),
        )
        .route(
            "/api/v1/workload_relay_mesh_secret",
            get(get_workload_relay_mesh_secret),
        )
        .route("/api/v1/proxy_grant", post(post_proxy_grant))
        .with_state(state);

    // Liveness stays outside the limiter so an orchestrator polling health is
    // never throttled by unrelated client traffic.
    let app = with_rate_limit(limited, rate_limit_per_minute).merge(
        Router::new()
            .route("/healthz", get(healthz))
            .with_state(health_state),
    );

    Ok(spawn_tcp_server(addr, app, "workload-rest-api"))
}

async fn healthz(State(state): State<RestState>) -> Json<HealthResponse> {
    let peers = state.peers.latest().await;
    let uptime = Instant::now()
        .saturating_duration_since(state.started_at)
        .as_secs();
    Json(HealthResponse {
        status: "ok",
        uptime_secs: uptime,
        peer_count: peers.len(),
    })
}

async fn get_peer_id(State(state): State<RestState>) -> Json<PeerIdResponse> {
    Json(PeerIdResponse {
        peer_id: state.local_peer_id.clone(),
    })
}

async fn get_endpoint_record(State(state): State<RestState>) -> impl IntoResponse {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let record = match state.endpoint_record.read() {
        Ok(record) => record.clone(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: "proxy EndpointRecord lock poisoned".into(),
                }),
            )
                .into_response();
        }
    };
    let signing_pubkey_b64 = record.signing_pubkey.clone();
    let peer_id = hex::encode(&record.endpoint_id);
    match record.to_bytes(now) {
        Ok(bytes) => (
            StatusCode::OK,
            Json(EndpointRecordResponse {
                endpoint_record_b64: crypto::b64_encode(&bytes),
                peer_id,
                signing_pubkey_b64,
            }),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: format!("failed to encode EndpointRecord: {error}"),
            }),
        )
            .into_response(),
    }
}

/// Body for `POST /api/v1/proxy_grant`.
#[derive(serde::Deserialize)]
struct PostProxyGrantBody {
    /// Base64 Ed25519 public key of the namespace owner that minted the grant.
    owner_pubkey_b64: String,
    /// Base64 Biscuit naming this proxy.
    grant_b64: String,
}

/// Accepts an owner-signed grant for this proxy.
///
/// The endpoint is deliberately unauthenticated: the grant authenticates
/// itself. It is only stored if it verifies against the owner key it names and
/// authorizes this proxy's own endpoint, so a caller can never insert authority
/// for an owner whose private key they do not hold.
async fn post_proxy_grant(
    State(state): State<RestState>,
    Json(body): Json<PostProxyGrantBody>,
) -> impl IntoResponse {
    let encoded = match protocol::proxy_grant_from_b64(&body.grant_b64) {
        Ok(encoded) => encoded,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!("failed to decode proxy grant: {error}"),
                }),
            )
                .into_response();
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    match state
        .grant_store
        .accept(&body.owner_pubkey_b64, encoded, &state.local_peer_id, now)
    {
        Ok(()) => (
            StatusCode::OK,
            Json(CertAck {
                ok: true,
                owner_pubkey: body.owner_pubkey_b64,
                message: "proxy grant accepted".to_string(),
            }),
        )
            .into_response(),
        Err(_error) => (
            StatusCode::BAD_REQUEST,
            // Only a generic reason is returned; the detail goes to the log,
            // because this endpoint is reachable by anyone.
            Json(ApiError {
                error: "proxy grant rejected".into(),
            }),
        )
            .into_response(),
    }
}
