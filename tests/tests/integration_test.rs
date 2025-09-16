use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;

struct NodeGuard {
    node1: Option<tokio::process::Child>,
    node2: Option<tokio::process::Child>,
}

impl NodeGuard {
    async fn cleanup(&mut self) {
        if let Some(mut n2) = self.node2.take() {
            let _ = n2.kill().await;
        }
        if let Some(mut n1) = self.node1.take() {
            let _ = n1.kill().await;
        }
    }
}

// Ensure processes are killed even if the test panics: Drop is called on unwind.
impl Drop for NodeGuard {
    fn drop(&mut self) {
        // Best-effort synchronous kill. tokio::process::Child exposes a synchronous kill().
        if let Some(mut n2) = self.node2.take() {
            let _ = n2.kill();
        }
        if let Some(mut n1) = self.node1.take() {
            let _ = n1.kill();
        }
    }
}

#[tokio::test]
async fn test_run_host_application() {
    let node1 = Command::new("../target/debug/host")
        .spawn()
        .expect("Failed to start host application");

    //sleep(Duration::from_secs(2)).await;

    let node2 = Command::new("../target/debug/host")
        .args(&["--disable-rest-api", "--disable-host-api"])
        .spawn()
        .expect("Failed to start host application");

    let mut guard = NodeGuard {
        node1: Some(node1),
        node2: Some(node2),
    };

    let _sleep = sleep(Duration::from_secs(5)).await;

    check_health().await;
    verify_peers().await;
    start_gateway().await;
    let _sleep = sleep(Duration::from_secs(1)).await;
    verify_gateway_health().await;
    stop_gateway().await;

    guard.cleanup().await;
}

async fn check_health() {
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            reqwest::get("http://localhost:3000/health")
        ).await.unwrap().unwrap();
        assert_eq!(resp.text().await.unwrap(), "ok");
}

async fn verify_peers() {
        let resp = tokio::time::timeout(
        Duration::from_secs(5),
        reqwest::get("http://localhost:3000/nodes")
    ).await.unwrap().unwrap();
    let nodes: serde_json::Value = resp.json().await.unwrap();
    let peers = nodes["peers"].as_array().expect("peers should be an array");
    assert!(!peers.is_empty(), "Expected at least one peer in the mesh, got {:?}", nodes);
}

async fn start_gateway() {
    let client = reqwest::Client::new();
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        client.post("http://localhost:3000/start_pod")
            .json(&serde_json::json!({"pod_name": "testpod"}))
            .send()
    ).await.unwrap().unwrap();
    let text = resp.text().await.unwrap_or_else(|e| panic!("Failed to read body: {}", e));
    let result: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => panic!("Failed to parse JSON: {}. Body: {}", e, text),
    };
    let socket_path = result["socket_path"].as_str().expect("socket_path should be a string");
    assert!(socket_path.contains("testpod"), "Expected socket path to contain pod name, got {}", socket_path);
}

async fn stop_gateway() {
    let client = reqwest::Client::new();
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        client.post("http://localhost:3000/stop_pod")
            .json(&serde_json::json!({"pod_name": "testpod"}))
            .send()
    ).await.unwrap().unwrap();
    let result: serde_json::Value = resp.json().await.unwrap();
    let status = result["status"].as_str().expect("status should be a string");
    assert_eq!(status, "stopped gateway", "Expected status to be 'stopped', got {}", status);
}

async fn verify_gateway_health() {
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        reqwest::get("http://localhost:3000/self/testpod/health")
    ).await.unwrap().unwrap();
    let text = resp.text().await.unwrap();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
        if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
            return;
        }
    }
    panic!("Expected gateway health OK, got {}", text);
}