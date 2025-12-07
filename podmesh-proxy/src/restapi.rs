use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use axum::{Json, Router, extract::State, routing::get};
use axum_support::{parse_socket_addr, spawn_tcp_server};
use serde::Serialize;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

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
    let addr = parse_socket_addr(&host, port)?;
    let state = RestState {
        started_at: Instant::now(),
        peers: PeerSnapshot::new(peer_rx),
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
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
