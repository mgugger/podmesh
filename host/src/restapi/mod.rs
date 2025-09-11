use axum::{routing::{post, get}, Router, Json, extract::State};
use serde::{Deserialize, Serialize};
use tokio::{process::Command, sync::watch};
use crate::pod_communication;

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
    .route("/init_pod", post(init_pod))
    .route("/stop_pod", post(stop_pod))
        .with_state(peer_rx)
}

#[derive(Deserialize)]
pub struct InitPodRequest {
    pub pod_name: String,
}

#[derive(Serialize)]
pub struct InitPodResponse {
    pub socket_path: String,
    pub status: String,
}

pub async fn init_pod(Json(req): Json<InitPodRequest>) -> Json<InitPodResponse> {
    match pod_communication::init_pod_listener(&req.pod_name).await {
        Ok(socket_path) => {
            // start gateway process and pass --host_socket
            let host_socket = socket_path.clone();
            // spawn the gateway using default binary location
            match pod_communication::start_gateway_for_pod(&req.pod_name, None, &host_socket).await {
                Ok(_) => Json(InitPodResponse { socket_path, status: "listener and gateway started".to_string() }),
                Err(e) => Json(InitPodResponse { socket_path, status: format!("listener started; gateway failed: {}", e) }),
            }
        }
        Err(e) => Json(InitPodResponse { socket_path: "".to_string(), status: e }),
    }
}

#[derive(Deserialize)]
pub struct StopPodRequest {
    pub pod_name: String,
}

pub async fn stop_pod(Json(req): Json<StopPodRequest>) -> Json<InitPodResponse> {
    match pod_communication::stop_gateway_for_pod(&req.pod_name).await {
        Ok(_) => Json(InitPodResponse { socket_path: "".to_string(), status: "stopped gateway".to_string() }),
        Err(e) => Json(InitPodResponse { socket_path: "".to_string(), status: format!("stop failed: {}", e) }),
    }
}