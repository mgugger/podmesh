use crate::pod_communication;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use beemesh_protocol::libp2p_constants::{
    FREE_CAPACITY_PREFIX, FREE_CAPACITY_TIMEOUT_SECS, REPLICAS_FIELD, SPEC_REPLICAS_FIELD,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::{sync::watch, time::Duration};

#[derive(Serialize)]
pub struct NodesResponse {
    pub peers: Vec<String>,
}

async fn get_nodes(State(state): State<RestState>) -> Json<NodesResponse> {
    let peers = state.peer_rx.borrow().clone();
    Json(NodesResponse { peers })
}

#[derive(Clone)]
pub struct RestState {
    pub peer_rx: watch::Receiver<Vec<String>>,
    pub control_tx: mpsc::UnboundedSender<crate::libp2p_beemesh::Libp2pControl>,
}

pub fn build_router(
    peer_rx: watch::Receiver<Vec<String>>,
    control_tx: mpsc::UnboundedSender<crate::libp2p_beemesh::Libp2pControl>,
) -> Router {
    let state = RestState {
        peer_rx,
        control_tx,
    };
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/tenants/{tenant}/apply", post(apply_manifest))
        .route("/{peer_id}/{pod_name}/health", get(peer_pod_health))
        .route("/nodes", get(get_nodes))
        // routes for testing
        .route("/start_pod", post(start_pod))
        .route("/stop_pod", post(stop_pod))
        // state
        .with_state(state)
}

// pub async fn init_raft(
//     Path((peer_id, pod_name)): Path<(String, String)>,
// ) -> Json<serde_json::Value> {
//     // If peer_id indicates local host, perform the unix-socket init_raft.
//     if peer_id == "local" || peer_id == "self" || peer_id == "0" {
//         // Gateway expects POST /init with a JSON array payload. For single-node init an empty
//         // array (`[]`) is accepted and the gateway will initialize using its own id/addr.
//         let body = serde_json::json!([]);
//         match pod_communication::send_request::<serde_json::Value>(&pod_name, "POST", "/init", Some(&body)).await {
//             Ok((status, _parsed, text)) => {
//                 if status.is_success() {
//                     Json(serde_json::json!({"ok": true, "status": "raft initialized", "body": text}))
//                 } else {
//                     Json(serde_json::json!({"ok": false, "error": format!("gateway returned {}", status), "body": text}))
//                 }
//             }
//             Err(e) => Json(serde_json::json!({"ok": false, "error": e})),
//         }
//     } else {
//         // Remote peer init_raft via libp2p are not implemented yet.
//         Json(serde_json::json!({"ok": false, "error": "peer init_raft not implemented"}))
//     }
// }

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

pub async fn apply_manifest(
    Path(tenant): Path<String>,
    State(state): State<RestState>,
    Json(manifest): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    println!("tenant: {:?}", tenant);
    /*println!(
        "apply_manifest received: {}",
        serde_json::to_string_pretty(&manifest).unwrap_or_default()
    );*/

    // determine desired replica count from manifest; check top-level `replicas` or `spec.replicas`
    let replicas = manifest
        .get(REPLICAS_FIELD)
        .and_then(|v| v.as_u64())
        .or_else(|| {
            manifest
                .get(SPEC_REPLICAS_FIELD)
                .and_then(|s| s.get("replicas"))
                .and_then(|r| r.as_u64())
        })
        .unwrap_or(1) as usize;

    // publish a QueryCapacity control message to the libp2p task and collect replies
    let request_id = format!("{}-{}", FREE_CAPACITY_PREFIX, uuid::Uuid::new_v4());
    // build a sample flatbuffer CapacityRequest (sample values for now)
    let capacity_fb = beemesh_protocol::flatbuffer::build_capacity_request(
        500u32,                    // cpu_milli
        512u64 * 1024 * 1024,      // memory_bytes (512MB)
        10u64 * 1024 * 1024 * 1024, // storage_bytes (10GB)
        replicas as u32,          // replicas
    );
    println!("apply_manifest: request_id={}, replicas={} payload_bytes={}", request_id, replicas, capacity_fb.len());
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<String>();
    let _ = state
        .control_tx
        .send(crate::libp2p_beemesh::Libp2pControl::QueryCapacityWithPayload {
            request_id: request_id.clone(),
            reply_tx: reply_tx.clone(),
            payload: capacity_fb,
        });

    // collect replies for the configured timeout (or until we have enough responders)
    let mut responders: Vec<String> = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(FREE_CAPACITY_TIMEOUT_SECS) {
        let remaining =
            Duration::from_secs(FREE_CAPACITY_TIMEOUT_SECS).saturating_sub(start.elapsed());
        match tokio::time::timeout(remaining, reply_rx.recv()).await {
            Ok(Some(peer)) => {
                if !responders.contains(&peer) {
                    responders.push(peer);
                }
                if responders.len() >= replicas {
                    break;
                }
            }
            _ => break, // timeout or closed
        }
    }

    println!("apply_manifest: collected {} responders", responders.len());

    let mut per_peer = serde_json::Map::new();

    // pick up to `replicas` peers from responders
    let assigned: Vec<String> = responders.into_iter().take(replicas).collect();
    if assigned.len() == 0 {
        return Json(serde_json::json!({
            "ok": false,
            "tenant": tenant,
            "replicas_requested": replicas,
            "assigned_peers": assigned,
            "per_peer": serde_json::Value::Object(per_peer),
        }));
    }

    // dispatch manifest to each assigned peer (stubbed)
    for peer in &assigned {
        match pod_communication::send_apply_to_peer(peer, &manifest).await {
            Ok(_) => {
                per_peer.insert(peer.clone(), serde_json::Value::String("ok".to_string()));
            }
            Err(e) => {
                per_peer.insert(
                    peer.clone(),
                    serde_json::Value::String(format!("error: {}", e)),
                );
            }
        }
    }

    Json(serde_json::json!({
        "ok": true,
        "tenant": tenant,
        "replicas_requested": replicas,
        "assigned_peers": assigned,
        "per_peer": serde_json::Value::Object(per_peer),
    }))
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
