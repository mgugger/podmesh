use env_logger::Env;
use serial_test::serial;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;

mod common;
use common::apply_common::{
    check_workload_deployment, get_peer_ids, setup_test_environment, start_cluster_nodes,
    wait_for_mesh_formation,
};
use common::test_utils::{NodeGuard, make_test_cli, setup_cleanup_hook, start_nodes};

fn manifest_path(file: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}/tests/sample_manifests/{}",
        env!("CARGO_MANIFEST_DIR"),
        file
    ))
}

#[serial]
#[tokio::test]
async fn test_apply_functionality() {
    let (client, ports) = setup_test_environment().await;
    let mut guard = start_cluster_nodes(&[false, false, false]).await;

    sleep(Duration::from_secs(3)).await;
    let mesh_formed = wait_for_mesh_formation(&client, &ports, Duration::from_secs(15)).await;
    if !mesh_formed {
        log::warn!("Mesh formation incomplete, but proceeding with test");
    }

    let manifest_path = manifest_path("nginx.yml");
    let original_content = tokio::fs::read_to_string(manifest_path.clone())
        .await
        .expect("Failed to read original manifest file for verification");

    let task_id = beectl::apply_file(manifest_path.clone(), None)
        .await
        .expect("apply_file should succeed");

    sleep(Duration::from_secs(6)).await;

    let port_to_peer_id = get_peer_ids(&client, &ports).await;
    let (nodes_with_deployed_workloads, nodes_with_content_mismatch) = check_workload_deployment(
        &client,
        &ports,
        &task_id,
        &original_content,
        &port_to_peer_id,
        false,
    )
    .await;

    assert_eq!(
        nodes_with_deployed_workloads.len(),
        1,
        "Expected exactly 1 node to have workload deployed with correct peer ID, found {:?}",
        nodes_with_deployed_workloads
    );

    assert!(
        nodes_with_content_mismatch.is_empty(),
        "Manifest content verification failed on nodes: {:?}",
        nodes_with_content_mismatch
    );

    guard.cleanup().await;
}

#[serial]
#[tokio::test]
async fn test_apply_with_real_podman() {
    if !is_podman_available().await {
        log::warn!("Skipping Podman integration test - Podman not available");
        return;
    }

    let (_client, _ports) = setup_test_environment_for_podman().await;
    let mut guard = start_test_nodes_for_podman().await;

    sleep(Duration::from_secs(3)).await;

    let manifest_path = manifest_path("nginx.yml");
    let original_content = tokio::fs::read_to_string(manifest_path.clone())
        .await
        .expect("Failed to read original manifest file for verification");

    let task_id = beectl::apply_file(manifest_path.clone(), None)
        .await
        .expect("apply_file should succeed with real Podman");

    sleep(Duration::from_secs(5)).await;

    let podman_verification_successful =
        verify_podman_deployment(&task_id, &original_content).await;

    assert!(
        podman_verification_successful,
        "Podman deployment verification failed - no matching pods found"
    );

    let _delete_result = beectl::delete_file(manifest_path, true, None).await;
    sleep(Duration::from_secs(5)).await;

    let podman_verification_successful =
        verify_podman_deployment(&task_id, &original_content).await;
    assert!(
        !podman_verification_successful,
        "Podman deployment still exists after deletion attempt"
    );

    cleanup_podman_resources(&task_id).await;
    guard.cleanup().await;
}

#[serial]
#[tokio::test]
async fn test_apply_nginx_with_replicas() {
    let (client, ports) = setup_test_environment().await;
    let mut guard = start_cluster_nodes(&[false, false, false]).await;

    sleep(Duration::from_secs(3)).await;
    let mesh_formed = wait_for_mesh_formation(&client, &ports, Duration::from_secs(5)).await;
    if !mesh_formed {
        log::warn!("Mesh formation incomplete, but proceeding with test");
    }

    let manifest_path = manifest_path("nginx_with_replicas.yml");
    let original_content = tokio::fs::read_to_string(manifest_path.clone())
        .await
        .expect("Failed to read nginx_with_replicas manifest file for verification");

    let task_id = beectl::apply_file(manifest_path.clone(), None)
        .await
        .expect("apply_file should succeed for nginx_with_replicas");

    sleep(Duration::from_secs(5)).await;

    let port_to_peer_id = get_peer_ids(&client, &ports).await;
    let (nodes_with_deployed_workloads, nodes_with_content_mismatch) = check_workload_deployment(
        &client,
        &ports,
        &task_id,
        &original_content,
        &port_to_peer_id,
        true,
    )
    .await;

    assert_eq!(
        nodes_with_deployed_workloads.len(),
        3,
        "Expected exactly 3 nodes to have workload deployed, found {:?}",
        nodes_with_deployed_workloads
    );

    let mut sorted_nodes = nodes_with_deployed_workloads.clone();
    sorted_nodes.sort();
    let mut expected_nodes = ports.clone();
    expected_nodes.sort();
    assert_eq!(
        sorted_nodes, expected_nodes,
        "Expected workloads on nodes {:?}, found {:?}",
        expected_nodes, sorted_nodes
    );

    assert!(
        nodes_with_content_mismatch.is_empty(),
        "Manifest content verification failed on nodes: {:?}",
        nodes_with_content_mismatch
    );

    log::info!(
        "✓ MockEngine verification passed: nginx_with_replicas manifest {} deployed on {:?}",
        task_id,
        nodes_with_deployed_workloads
    );

    guard.cleanup().await;
}

async fn setup_test_environment_for_podman() -> (reqwest::Client, Vec<u16>) {
    setup_cleanup_hook();
    let _ = env_logger::Builder::from_env(Env::default().default_filter_or("warn")).try_init();

    (reqwest::Client::new(), vec![3000u16, 3100u16, 3200u16])
}

async fn start_test_nodes_for_podman() -> NodeGuard {
    let mut cli1 = make_test_cli(3000, false, true, None, vec![], 4001, false);
    cli1.mock_only_runtime = false;
    cli1.signing_ephemeral = false;
    cli1.kem_ephemeral = false;
    cli1.ephemeral_keys = false;

    let mut cli2 = make_test_cli(
        3100,
        false,
        true,
        None,
        vec!["/ip4/127.0.0.1/udp/4001/quic-v1".to_string()],
        4002,
        false,
    );
    cli2.mock_only_runtime = false;
    cli2.signing_ephemeral = false;
    cli2.kem_ephemeral = false;
    cli2.ephemeral_keys = false;

    let bootstrap_peers = vec![
        "/ip4/127.0.0.1/udp/4001/quic-v1".to_string(),
        "/ip4/127.0.0.1/udp/4002/quic-v1".to_string(),
    ];

    let mut cli3 = make_test_cli(3200, false, true, None, bootstrap_peers, 0, false);
    cli3.mock_only_runtime = false;
    cli3.signing_ephemeral = false;
    cli3.kem_ephemeral = false;
    cli3.ephemeral_keys = false;

    start_nodes(vec![cli1, cli2, cli3], Duration::from_secs(1)).await
}

async fn is_podman_available() -> bool {
    match tokio::process::Command::new("podman")
        .args(["--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

async fn verify_podman_deployment(task_id: &str, _original_content: &str) -> bool {
    let expected_pod_name = format!("beemesh-{}-pod", task_id);

    let output = tokio::process::Command::new("podman")
        .args(["pod", "ls", "--format", "json"])
        .output()
        .await;

    if let Ok(output) = output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);

            if let Ok(pods) = serde_json::from_str::<serde_json::Value>(&stdout) {
                if let Some(pods_array) = pods.as_array() {
                    for pod in pods_array {
                        if let Some(name) = pod.get("Name").and_then(|n| n.as_str()) {
                            if name == expected_pod_name {
                                log::info!("Found matching Podman pod: {}", name);
                                return true;
                            }
                        }
                    }
                }
            }

            let container_output = tokio::process::Command::new("podman")
                .args(["ps", "-a", "--format", "json"])
                .output()
                .await;

            if let Ok(container_output) = container_output {
                if container_output.status.success() {
                    let container_stdout = String::from_utf8_lossy(&container_output.stdout);
                    if let Ok(containers) =
                        serde_json::from_str::<serde_json::Value>(&container_stdout)
                    {
                        if let Some(containers_array) = containers.as_array() {
                            for container in containers_array {
                                if let Some(names) =
                                    container.get("Names").and_then(|n| n.as_array())
                                {
                                    for name in names {
                                        if let Some(name_str) = name.as_str() {
                                            if name_str.contains(&format!("beemesh-{}", task_id)) {
                                                log::info!(
                                                    "Found matching Podman container: {}",
                                                    name_str
                                                );
                                                return true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    } else {
        log::warn!("Failed to execute 'podman pod ls' command");
    }

    false
}

async fn cleanup_podman_resources(task_id: &str) {
    log::info!("Cleaning up Podman resources for task: {}", task_id);

    let expected_pod_name = format!("beemesh-{}-pod", task_id);
    let _ = tokio::process::Command::new("podman")
        .args(["pod", "rm", "-f", &expected_pod_name])
        .output()
        .await;

    let expected_pod_name_alt = format!("beemesh-{}", task_id);
    let _ = tokio::process::Command::new("podman")
        .args(["pod", "rm", "-f", &expected_pod_name_alt])
        .output()
        .await;

    if let Ok(output) = tokio::process::Command::new("podman")
        .args(["pod", "ls", "-q", "--filter", "name=beemesh"])
        .output()
        .await
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let pod_id = line.trim();
                if !pod_id.is_empty() {
                    let _ = tokio::process::Command::new("podman")
                        .args(["pod", "rm", "-f", pod_id])
                        .output()
                        .await;
                }
            }
        }
    }

    if let Ok(container_output) = tokio::process::Command::new("podman")
        .args(["ps", "-aq", "--filter", "name=beemesh"])
        .output()
        .await
    {
        if container_output.status.success() {
            let stdout = String::from_utf8_lossy(&container_output.stdout);
            for line in stdout.lines() {
                let container_id = line.trim();
                if !container_id.is_empty() {
                    let _ = tokio::process::Command::new("podman")
                        .args(["rm", "-f", container_id])
                        .output()
                        .await;
                }
            }
        }
    }
}
