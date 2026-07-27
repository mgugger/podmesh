use std::collections::HashMap;
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
use axum_support::{parse_socket_addr, spawn_tcp_server};
use protocol::{NodeCert, NodeRole};
use serde::Serialize;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

/// Shared store of tenant-issued NodeCerts held by the proxy.
/// Keyed by `owner_pubkey` (base64 Ed25519 public key).
pub type CertStore = Arc<RwLock<HashMap<String, NodeCert>>>;

/// Construct a fresh, empty cert store.
pub fn new_cert_store() -> CertStore {
    Arc::new(RwLock::new(HashMap::new()))
}

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

/// Inputs to the REST server.
pub struct RestServerOptions {
    pub host: String,
    pub port: u16,
    pub peer_rx: watch::Receiver<Vec<String>>,
    /// The proxy's libp2p PeerId — used to validate that incoming NodeCerts
    /// were issued for this proxy.
    pub local_peer_id: String,
    /// Shared store for cert persistence + lookup.
    pub cert_store: CertStore,
}

#[derive(Clone)]
struct RestState {
    started_at: Instant,
    peers: PeerSnapshot,
    local_peer_id: String,
    cert_store: CertStore,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    uptime_secs: u64,
    peer_count: usize,
}

#[derive(Serialize)]
struct PubkeyResponse {
    pubkey_b64: String,
}

#[derive(Serialize)]
struct PeerIdResponse {
    peer_id: String,
}

#[derive(Serialize)]
struct CertAck {
    ok: bool,
    owner_pubkey: String,
    valid_until: u64,
    message: String,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

pub fn spawn_rest_server(options: RestServerOptions) -> Result<JoinHandle<()>> {
    let RestServerOptions {
        host,
        port,
        peer_rx,
        local_peer_id,
        cert_store,
    } = options;

    let addr = parse_socket_addr(&host, port)?;
    let state = RestState {
        started_at: Instant::now(),
        peers: PeerSnapshot::new(peer_rx),
        local_peer_id,
        cert_store,
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/peer_id", get(get_peer_id))
        .route("/api/v1/signing_pubkey", get(get_signing_pubkey))
        .route("/api/v1/kem_pubkey", get(get_kem_pubkey))
        .route("/api/v1/node_cert", post(post_node_cert))
        .with_state(state);

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

async fn get_signing_pubkey(State(_state): State<RestState>) -> impl IntoResponse {
    match crypto::ensure_keypair_on_disk() {
        Ok((pub_bytes, _)) => (
            StatusCode::OK,
            Json(PubkeyResponse {
                pubkey_b64: crypto::b64_encode(&pub_bytes),
            }),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: format!("failed to load signing keypair: {}", err),
            }),
        )
            .into_response(),
    }
}

async fn get_kem_pubkey(State(_state): State<RestState>) -> impl IntoResponse {
    match crypto::ensure_kem_keypair_on_disk() {
        Ok((pub_bytes, _)) => (
            StatusCode::OK,
            Json(PubkeyResponse {
                pubkey_b64: crypto::b64_encode(&pub_bytes),
            }),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: format!("failed to load kem keypair: {}", err),
            }),
        )
            .into_response(),
    }
}

/// Body for `POST /api/v1/node_cert`.
#[derive(serde::Deserialize)]
struct PostNodeCertBody {
    /// Base64-postcard `NodeCert`.
    cert_b64: String,
}

async fn post_node_cert(
    State(state): State<RestState>,
    Json(body): Json<PostNodeCertBody>,
) -> impl IntoResponse {
    let cert = match NodeCert::from_b64(&body.cert_b64) {
        Ok(c) => c,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!("failed to decode NodeCert: {}", err),
                }),
            )
                .into_response();
        }
    };

    if let Err(err) = cert.verify() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("NodeCert signature verification failed: {}", err),
            }),
        )
            .into_response();
    }

    if cert.peer_id != state.local_peer_id {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!(
                    "NodeCert.peer_id={} does not match this proxy's PeerId={}",
                    cert.peer_id, state.local_peer_id
                ),
            }),
        )
            .into_response();
    }

    if cert.role != NodeRole::Proxy {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("expected NodeRole::Proxy, got {:?}", cert.role),
            }),
        )
            .into_response();
    }

    if cert.is_expired() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "NodeCert is expired".to_string(),
            }),
        )
            .into_response();
    }

    let owner_pubkey = cert.owner_pubkey.clone();
    let valid_until = cert.valid_until;
    {
        let mut store = match state.cert_store.write() {
            Ok(g) => g,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError {
                        error: format!("cert store lock poisoned: {}", e),
                    }),
                )
                    .into_response();
            }
        };
        store.insert(owner_pubkey.clone(), cert);
    }

    (
        StatusCode::OK,
        Json(CertAck {
            ok: true,
            owner_pubkey,
            valid_until,
            message: "NodeCert accepted".to_string(),
        }),
    )
        .into_response()
}
