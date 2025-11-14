use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

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

#[derive(Clone)]
struct RestState {
    started_at: Instant,
    peers: PeerSnapshot,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    uptime_secs: u64,
    peer_count: usize,
}

pub fn spawn_rest_server(
    host: String,
    port: u16,
    peer_rx: watch::Receiver<Vec<String>>,
) -> Result<JoinHandle<()>> {
    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    let state = RestState {
        started_at: Instant::now(),
        peers: PeerSnapshot::new(peer_rx),
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .with_state(state);

    let handle = tokio::spawn(async move {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                info!(
                    "workload rest api listening on {}",
                    listener.local_addr().unwrap()
                );
                if let Err(err) = axum::serve(listener, app).await {
                    warn!("workload rest api stopped: {}", err);
                }
            }
            Err(err) => warn!("failed to bind workload rest api {}: {}", addr, err),
        }
    });

    Ok(handle)
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
