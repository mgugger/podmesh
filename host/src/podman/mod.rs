use axum::{Json, routing::post, Router};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

#[derive(Deserialize)]
pub struct PodRequest {
    pub pod_name: String,
}

#[derive(Serialize)]
pub struct PodResponse {
    pub socket_path: String,
    pub status: String,
}

pub async fn create_podman_socket(Json(req): Json<PodRequest>) -> Json<PodResponse> {
    let pod_name = &req.pod_name;
    let socket_path = format!("/run/podmesh/pod_{}.sock", pod_name);
    let pod_status = match Command::new("podman")
        .args(["pod", "create", "--name", pod_name])
        .output()
        .await {
        Ok(output) if output.status.success() => "created".to_string(),
        Ok(output) => format!("failed: {}", String::from_utf8_lossy(&output.stderr)),
        Err(e) => format!("error: {}", e),
    };

    // Start Podman API service for this pod (background)
    let service_status = match Command::new("podman")
        .args(["system", "service", "--time=0", &format!("unix:{}", socket_path)])
        .spawn() {
        Ok(_child) => "podman service started".to_string(),
        Err(e) => format!("podman service error: {}", e),
    };

    let status = format!("{}; {}", pod_status, service_status);
    Json(PodResponse {
        socket_path,
        status,
    })
}

pub fn build_router() -> Router {
    Router::new().route("/podman/socket", post(create_podman_socket))
}
