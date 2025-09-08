use axum::{routing::{post, get}, Router, Json, extract::State};
use serde::{Deserialize, Serialize};
use tokio::{process::Command, sync::watch};

#[derive(Deserialize)]
pub struct ExecRequest {
    pub command: String,
    pub args: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct ExecResponse {
    pub stdout: String,
    pub stderr: String,
    pub status: i32,
}

pub async fn exec_command(Json(req): Json<ExecRequest>) -> Json<ExecResponse> {
    let mut cmd = Command::new(&req.command);
    if let Some(args) = &req.args {
        cmd.args(args);
    }
    let output = cmd.output().await.unwrap();
    Json(ExecResponse {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        status: output.status.code().unwrap_or(-1),
    })
}

#[derive(Serialize)]
pub struct NodesResponse {
    pub peers: Vec<String>,
}

async fn get_nodes(State(peer_rx): State<watch::Receiver<Vec<String>>>) -> Json<NodesResponse> {
    let peers = peer_rx.borrow().clone();
    Json(NodesResponse { peers })
}

pub fn build_router(peer_rx: watch::Receiver<Vec<String>>) -> Router {
    Router::new()
        .route("/exec", post(exec_command))
        .route("/health", get(|| async { "ok" }))
        .route("/nodes", get(get_nodes))
        .route("/init_raft", get(|| async { "not implemented" }))
        .with_state(peer_rx)
}