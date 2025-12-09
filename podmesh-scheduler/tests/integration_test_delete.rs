use env_logger::Env;
use serial_test::serial;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;

mod common;
use common::test_utils::{NodeGuard, make_test_cli, set_env_var, setup_cleanup_hook, start_nodes};

fn manifest_path() -> PathBuf {
    PathBuf::from(format!(
        "{}/tests/sample_manifests/nginx.yml",
        env!("CARGO_MANIFEST_DIR")
    ))
}

async fn setup_test_environment() -> Vec<u16> {
    setup_cleanup_hook();
    let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info")).try_init();
    vec![3000u16, 3100u16]
}

async fn start_test_nodes() -> NodeGuard {
    let cli1 = make_test_cli(3000, false, true, None, vec![], 4001, false);
    let cli2 = make_test_cli(
        3100,
        false,
        true,
        None,
        vec!["/ip4/127.0.0.1/udp/4001/quic-v1".to_string()],
        4002,
        false,
    );

    start_nodes(vec![cli1, cli2], Duration::from_secs(1)).await
}

#[tokio::test]
#[serial]
async fn test_delete_task_endpoint() {
    let _ports = setup_test_environment().await;
    let _node_guard = start_test_nodes().await;

    sleep(Duration::from_secs(2)).await;
    set_env_var("PODMESH_API", "http://127.0.0.1:3000");

    let manifest_path = manifest_path();
    let task_id_result = podctl::apply_file(manifest_path.clone(), None).await;
    println!("Apply result: {:?}", task_id_result);

    sleep(Duration::from_millis(500)).await;

    let delete_result = podctl::delete_file(manifest_path, false, None).await;

    let manifest_id = delete_result.expect("Delete CLI command should succeed");
    println!("Delete CLI command succeeded with manifest_id: {}", manifest_id);
    assert!(!manifest_id.is_empty());
}

#[tokio::test]
#[serial]
async fn test_delete_task_with_force() {
    let _ports = setup_test_environment().await;
    let _node_guard = start_test_nodes().await;

    sleep(Duration::from_secs(2)).await;
    set_env_var("PODMESH_API", "http://127.0.0.1:3000");

    let manifest_path = manifest_path();
    let task_id_result = podctl::apply_file(manifest_path.clone(), None).await;
    println!("Apply result: {:?}", task_id_result);

    sleep(Duration::from_millis(500)).await;

    let delete_result = podctl::delete_file(manifest_path, true, None).await;

    let manifest_id = delete_result.expect("Force delete CLI command should succeed");
    println!("Force delete CLI command succeeded with manifest_id: {}", manifest_id);
    assert!(!manifest_id.is_empty());
}

/// Start nodes configured for remote delete testing:
/// - Node 1 (port 3000): Bootstrap node with scheduling DISABLED (just forwards requests)
/// - Node 2 (port 3100): Worker node with scheduling ENABLED (actually deploys workloads)
/// This ensures delete requests must be forwarded to the remote worker node.
async fn start_remote_delete_test_nodes() -> NodeGuard {
    // Bootstrap node: disable scheduling so it won't be a candidate
    let cli1 = make_test_cli(
        3000,
        false,  // disable_rest: false - we need REST API
        true,   // disable_machine: true
        None,
        vec![],
        4001,
        true,   // disable_scheduling: true - this node won't respond to capacity queries
    );
    // Worker node: enable scheduling so it will be the only candidate
    let cli2 = make_test_cli(
        3100,
        true,   // disable_rest: true - no REST API needed
        true,   // disable_machine: true
        None,
        vec!["/ip4/127.0.0.1/udp/4001/quic-v1".to_string()],
        4002,
        false,  // disable_scheduling: false - this node will respond to capacity queries
    );

    start_nodes(vec![cli1, cli2], Duration::from_secs(1)).await
}

/// Test delete operation where the workload is on a REMOTE node.
/// This exercises the full end-to-end encrypted delete path:
/// 1. Apply goes to bootstrap (port 3000), which forwards to worker (port 3100)
/// 2. Worker deploys workload and announces as provider
/// 3. Delete goes to bootstrap, discovers provider via DHT, sends encrypted delete to worker
/// This catches bugs like "Invalid recipient public key size" that only occur in remote scenarios.
#[tokio::test]
#[serial]
async fn test_delete_on_remote_node() {
    let _ports = setup_test_environment().await;
    let _node_guard = start_remote_delete_test_nodes().await;

    // Wait for nodes to connect and establish DHT
    sleep(Duration::from_secs(3)).await;
    set_env_var("PODMESH_API", "http://127.0.0.1:3000");

    let manifest_path = manifest_path();
    
    // Apply should succeed - workload goes to the worker node (not bootstrap)
    let task_id_result = podctl::apply_file(manifest_path.clone(), None).await;
    let manifest_id = task_id_result.expect("Apply should succeed");
    println!("Apply result: manifest_id={}", manifest_id);

    // Wait for DHT provider announcement to propagate
    sleep(Duration::from_secs(2)).await;

    // Delete should succeed - requires fetching provider's KEM public key from DHT
    // and sending encrypted delete request to the remote worker node
    let delete_result = podctl::delete_file(manifest_path, false, None).await;

    let deleted_manifest_id = delete_result.expect("Remote delete should succeed");
    println!("Remote delete succeeded with manifest_id: {}", deleted_manifest_id);
    assert_eq!(manifest_id, deleted_manifest_id);
}

#[tokio::test]
#[serial]
async fn test_delete_nonexistent_task() {
    let _ports = setup_test_environment().await;
    let _node_guard = start_test_nodes().await;

    sleep(Duration::from_secs(5)).await;
    set_env_var("PODMESH_API", "http://127.0.0.1:3000");

    let manifest_path = manifest_path();

    let delete_result = podctl::delete_file(manifest_path, false, None).await;

    // Deleting a nonexistent task should succeed (returns manifest_id with no providers found)
    let manifest_id = delete_result.expect("Delete nonexistent task should succeed");
    println!("Delete nonexistent task succeeded with manifest_id: {}", manifest_id);
    assert!(!manifest_id.is_empty());
}
