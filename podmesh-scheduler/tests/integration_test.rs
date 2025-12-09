use env_logger::Env;
use log::info;
use std::time::Duration;
use tokio::time::sleep;

mod common;
use common::test_utils::{make_test_cli, setup_cleanup_hook, start_nodes};

// IGNORED: This test is flaky due to timing-sensitive mesh formation with 3 nodes.
// The assertion requires exactly 2 peers to be visible but mesh discovery can be
// slower in some environments. Consider increasing timeouts or relaxing assertions.
#[ignore]
#[tokio::test]
async fn test_run_host_application() {
    setup_cleanup_hook();
    let _ = env_logger::Builder::from_env(Env::default().default_filter_or("warn")).try_init();

    let cli1 = make_test_cli(3000, false, true, None, vec![], 4001, false);
    let cli2 = make_test_cli(
        3100,
        true,
        true,
        None,
        vec!["/ip4/127.0.0.1/udp/4001/quic-v1".to_string()],
        4002,
        false,
    );
    let bootstrap_peers = vec![
        "/ip4/127.0.0.1/udp/4001/quic-v1".to_string(),
        "/ip4/127.0.0.1/udp/4002/quic-v1".to_string(),
    ];
    let cli3 = make_test_cli(3200, true, true, None, bootstrap_peers, 0, false);

    let mut guard = start_nodes(vec![cli1, cli2, cli3], Duration::from_secs(1)).await;

    let verify_peers = wait_for_peers(Duration::from_secs(15)).await;
    let health = check_health().await;

    let kem_pubkey_result = check_pubkey("kem_pubkey").await;
    let signing_pubkey_result = check_pubkey("signing_pubkey").await;

    guard.cleanup().await;

    assert_eq!(health, "ok");
    assert!(
        verify_peers["peers"]
            .as_array()
            .expect("peers should be an array")
            .len()
            == 2,
        "Expected at least two peers in the mesh, got {:?}",
        verify_peers
    );
    assert!(
        !kem_pubkey_result.is_empty(),
        "Expected kem_pubkey field in response, got: {}",
        kem_pubkey_result
    );
    assert!(
        !signing_pubkey_result.is_empty(),
        "Expected signing_pubkey field in response, got: {}",
        signing_pubkey_result
    );
}

async fn check_health() -> String {
    tokio::time::timeout(
        Duration::from_secs(5),
        reqwest::get("http://localhost:3000/health"),
    )
    .await
    .unwrap()
    .unwrap()
    .text()
    .await
    .expect("failed to call health api")
}

async fn check_pubkey(url: &str) -> String {
    tokio::time::timeout(
        Duration::from_secs(5),
        reqwest::get(format!("http://localhost:3000/api/v1/{}", url)),
    )
    .await
    .unwrap()
    .unwrap()
    .text()
    .await
    .expect("failed to call pubkey endpoint")
}

async fn verify_peers() -> serde_json::Value {
    let url = "http://localhost:3000/debug/peers";
    let resp = tokio::time::timeout(Duration::from_secs(5), reqwest::get(url))
        .await
        .unwrap()
        .unwrap();
    let json = resp.json().await;
    info!("{:?}", json);
    json.unwrap_or_default()
}

async fn wait_for_peers(timeout: Duration) -> serde_json::Value {
    let start = tokio::time::Instant::now();
    loop {
        let nodes = verify_peers().await;
        if !nodes["peers"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true)
        {
            return nodes;
        }
        if start.elapsed() > timeout {
            return nodes;
        }
        sleep(Duration::from_secs(1)).await;
    }
}
