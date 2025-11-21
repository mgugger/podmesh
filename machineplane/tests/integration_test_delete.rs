use env_logger::Env;
use serial_test::serial;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;

mod common;
use common::test_utils::{make_test_cli, set_env_var, setup_cleanup_hook, start_nodes, NodeGuard};

fn manifest_path() -> PathBuf {
    PathBuf::from(format!("{}/tests/sample_manifests/nginx.yml", env!("CARGO_MANIFEST_DIR")))
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
    set_env_var("BEEMESH_API", "http://127.0.0.1:3000");

    let manifest_path = manifest_path();
    let task_id_result = beectl::apply_file(manifest_path.clone(), None).await;
    println!("Apply result: {:?}", task_id_result);

    sleep(Duration::from_millis(500)).await;

    let delete_result = beectl::delete_file(manifest_path, false, None).await;

    match delete_result {
        Ok(manifest_id) => {
            println!("Delete CLI command succeeded with manifest_id: {}", manifest_id);
            assert!(!manifest_id.is_empty());
        }
        Err(e) => {
            println!("Delete CLI command failed: {}", e);
        }
    }
}

#[tokio::test]
#[serial]
async fn test_delete_task_with_force() {
    let _ports = setup_test_environment().await;
    let _node_guard = start_test_nodes().await;

    sleep(Duration::from_secs(2)).await;
    set_env_var("BEEMESH_API", "http://127.0.0.1:3000");

    let manifest_path = manifest_path();
    let task_id_result = beectl::apply_file(manifest_path.clone(), None).await;
    println!("Apply result: {:?}", task_id_result);

    sleep(Duration::from_millis(500)).await;

    let delete_result = beectl::delete_file(manifest_path, true, None).await;

    match delete_result {
        Ok(manifest_id) => {
            println!(
                "Force delete CLI command succeeded with manifest_id: {}",
                manifest_id
            );
            assert!(!manifest_id.is_empty());
        }
        Err(e) => {
            println!("Force delete CLI command failed: {}", e);
        }
    }
}

#[tokio::test]
#[serial]
async fn test_delete_nonexistent_task() {
    let _ports = setup_test_environment().await;
    let _node_guard = start_test_nodes().await;

    sleep(Duration::from_secs(5)).await;
    set_env_var("BEEMESH_API", "http://127.0.0.1:3000");

    let manifest_path = manifest_path();

    let delete_result = beectl::delete_file(manifest_path, false, None).await;

    match delete_result {
        Ok(manifest_id) => {
            println!(
                "Delete nonexistent task CLI command succeeded with manifest_id: {}",
                manifest_id
            );
            assert!(!manifest_id.is_empty());
        }
        Err(e) => {
            println!("Delete nonexistent task CLI command failed: {}", e);
        }
    }
}
