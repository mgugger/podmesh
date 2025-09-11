use crate::pod_communication;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
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
        .route("/gateway_health", get(gateway_health))
        .route("/{peer_id}/{pod_name}/health", get(peer_pod_health))
        .route("/{peer_id}/{pod_name}/init_raft", post(init_raft))
        .route("/nodes", get(get_nodes))
        .route("/start_pod", post(start_pod))
        .route("/stop_pod", post(stop_pod))
        .with_state(peer_rx)
}

#[derive(Deserialize)]
pub struct GatewayHealthQuery {
    pub pod_name: String,
    pub peer_id: Option<String>,
}

pub async fn init_raft(
    Path((peer_id, pod_name)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    // If peer_id indicates local host, perform the unix-socket init_raft.
    if peer_id == "local" || peer_id == "self" || peer_id == "0" {
        // Gateway expects POST /init with a JSON array payload. For single-node init an empty
        // array (`[]`) is accepted and the gateway will initialize using its own id/addr.
        let body = serde_json::json!([]);
        match pod_communication::send_request::<serde_json::Value>(&pod_name, "POST", "/init", Some(&body)).await {
            Ok((status, _parsed, text)) => {
                if status.is_success() {
                    Json(serde_json::json!({"ok": true, "status": "raft initialized", "body": text}))
                } else {
                    Json(serde_json::json!({"ok": false, "error": format!("gateway returned {}", status), "body": text}))
                }
            }
            Err(e) => Json(serde_json::json!({"ok": false, "error": e})),
        }
    } else {
        // Remote peer init_raft via libp2p are not implemented yet.
        Json(serde_json::json!({"ok": false, "error": "peer init_raft not implemented"}))
    }
}

pub async fn gateway_health(
    axum::extract::Query(q): axum::extract::Query<GatewayHealthQuery>,
) -> Json<serde_json::Value> {
    // if peer_id is provided, we could route via libp2p — not implemented yet
    if let Some(_peer) = q.peer_id.clone() {
        return Json(serde_json::json!({"ok": false, "error": "peer checks not implemented"}));
    }

    match pod_communication::send_health_check(&q.pod_name).await {
        Ok(ok) => Json(serde_json::json!({"ok": ok})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e})),
    }
}

pub async fn peer_pod_health(
    Path((peer_id, pod_name)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    // If peer_id indicates local host, perform the unix-socket health check.
    if peer_id == "local" || peer_id == "self" || peer_id == "0" {
        match pod_communication::send_health_check(&pod_name).await {
            Ok(ok) => Json(serde_json::json!({"ok": ok})),
            Err(e) => Json(serde_json::json!({"ok": false, "error": e})),
        }
    } else {
        // Remote peer checks via libp2p are not implemented yet.
        Json(serde_json::json!({"ok": false, "error": "peer checks not implemented"}))
    }
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

pub async fn start_pod(Json(req): Json<InitPodRequest>) -> Json<InitPodResponse> {
    match pod_communication::init_pod_listener(&req.pod_name).await {
        Ok(socket_path) => {
            // start gateway process and pass --host_socket
            let host_socket = socket_path.clone();
            // spawn the gateway using default binary location
            match pod_communication::start_gateway_for_pod(&req.pod_name, None, &host_socket).await
            {
                Ok(_) => Json(InitPodResponse {
                    socket_path,
                    status: "listener and gateway started".to_string(),
                }),
                Err(e) => Json(InitPodResponse {
                    socket_path,
                    status: format!("listener started; gateway failed: {}", e),
                }),
            }
        }
        Err(e) => Json(InitPodResponse {
            socket_path: "".to_string(),
            status: e,
        }),
    }
}

#[derive(Deserialize)]
pub struct StopPodRequest {
    pub pod_name: String,
}

pub async fn stop_pod(Json(req): Json<StopPodRequest>) -> Json<InitPodResponse> {
    match pod_communication::stop_gateway_for_pod(&req.pod_name).await {
        Ok(_) => Json(InitPodResponse {
            socket_path: "".to_string(),
            status: "stopped gateway".to_string(),
        }),
        Err(e) => Json(InitPodResponse {
            socket_path: "".to_string(),
            status: format!("stop failed: {}", e),
        }),
    }
}
