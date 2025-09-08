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

#[tokio::test]
async fn test_run_host_application() {
    let node1 = Command::new("../target/debug/host")
        .spawn()
        .expect("Failed to start host application");

    sleep(Duration::from_secs(2)).await;

    let node2 = Command::new("../target/debug/host")
        .args(&["--disable-rest-api"])
        .spawn()
        .expect("Failed to start host application");

    let mut guard = NodeGuard {
        node1: Some(node1),
        node2: Some(node2),
    };

    let test_result = async {
        sleep(Duration::from_secs(2)).await;

        // Check health
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            reqwest::get("http://localhost:3000/health")
        ).await.unwrap().unwrap();
        assert_eq!(resp.text().await.unwrap(), "ok");
        
        // check peers
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            reqwest::get("http://localhost:3000/nodes")
        ).await.unwrap().unwrap();
        let nodes: serde_json::Value = resp.json().await.unwrap();
        let peers = nodes["peers"].as_array().expect("peers should be an array");
        assert!(!peers.is_empty(), "Expected at least one peer in the mesh, got {:?}", nodes);

        Ok::<(), ()>(())
    }.await;
    guard.cleanup().await;
    test_result.unwrap();
}
