use serial_test::serial;
use std::time::Duration;
use std::{env, path::PathBuf};
use tokio::time::sleep;

mod common;
use common::apply_common::{
    check_workload_deployment, get_peer_ids, setup_test_environment, start_cluster_nodes,
    wait_for_mesh_formation,
};

#[cfg(feature = "podman-tests")]
use env_logger::Env;
#[cfg(feature = "podman-tests")]
use podmesh_scheduler::sidecar::SIDECAR_CONTAINER_NAME;
#[cfg(feature = "podman-tests")]
use common::test_utils::{cleanup_key_files, NodeGuard, make_test_cli, setup_cleanup_hook, start_nodes};

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

    let task_id = podctl::apply_file(manifest_path.clone(), None)
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
#[cfg(feature = "podman-tests")]
async fn test_apply_with_real_podman() {
    if !is_podman_available().await {
        log::warn!("Skipping Podman integration test - Podman not available");
        return;
    }

    let (_client, _ports) = setup_test_environment_for_podman().await;
    let mut guard = start_test_nodes_for_podman().await;

    sleep(Duration::from_secs(3)).await;

    let nginx_manifest_path = manifest_path("nginx.yml");
    let task_id = podctl::apply_file(nginx_manifest_path.clone(), None)
        .await
        .expect("apply_file should succeed with real Podman");

    sleep(Duration::from_secs(5)).await;

    let nginx_status = verify_podman_deployment(&task_id).await;
    if !nginx_status.workload_found {
        log::warn!(
            "Podman deployment verification failed for nginx task {}",
            task_id
        );
    }
    assert!(
        nginx_status.workload_found,
        "Podman deployment verification failed - no matching pods found"
    );
    assert!(
        nginx_status.sidecar_running,
        "Sidecar '{}' for task {} not running (state={:?})",
        SIDECAR_CONTAINER_NAME, task_id, nginx_status.sidecar_state
    );

    let _delete_result = podctl::delete_file(nginx_manifest_path, true, None).await;
    sleep(Duration::from_secs(5)).await;

    let nginx_removed = verify_podman_deployment(&task_id).await;
    if nginx_removed.workload_found {
        log::warn!(
            "Podman deployment still running for nginx task {} during cleanup",
            task_id
        );
    }
    assert!(
        !nginx_removed.workload_found,
        "Podman deployment still exists after deletion attempt"
    );

    cleanup_podman_resources(&task_id).await;

    let demo_manifest_path = manifest_path("demo_deployment.yml");
    let demo_task_id = podctl::apply_file(demo_manifest_path.clone(), None)
        .await
        .expect("apply_file should succeed for demo manifest");

    sleep(Duration::from_secs(5)).await;

    let demo_status = verify_podman_deployment(&demo_task_id).await;
    if !demo_status.workload_found {
        log::warn!(
            "Demo deployment verification failed for task {}",
            demo_task_id
        );
    }
    assert!(
        demo_status.workload_found,
        "Demo deployment verification failed for task {}",
        demo_task_id
    );
    assert!(
        demo_status.sidecar_running,
        "Sidecar '{}' for task {} not running (state={:?})",
        SIDECAR_CONTAINER_NAME, demo_task_id, demo_status.sidecar_state
    );

    let _delete_demo = podctl::delete_file(demo_manifest_path, true, None).await;
    sleep(Duration::from_secs(5)).await;

    let demo_removed = verify_podman_deployment(&demo_task_id).await;
    if demo_removed.workload_found {
        log::warn!(
            "Demo deployment still running for task {} during cleanup",
            demo_task_id
        );
    }
    assert!(
        !demo_removed.workload_found,
        "Demo deployment still exists after deletion attempt"
    );

    cleanup_podman_resources(&demo_task_id).await;
    guard.cleanup().await;
}

#[serial]
#[tokio::test]
#[ignore]
async fn test_apply_nginx_with_replicas() {
    let (client, ports) = setup_test_environment().await;
    let mut guard = start_cluster_nodes(&[false, false, false]).await;

    // Wait longer for mesh formation and stabilization, especially for replica tests
    sleep(Duration::from_secs(3)).await;
    let mesh_formed = wait_for_mesh_formation(&client, &ports, Duration::from_secs(20)).await;
    if !mesh_formed {
        log::warn!("Mesh formation incomplete, but proceeding with test");
    }
    
    // Additional stabilization time for request-response protocol to be ready
    // This is especially important in CI environments where resources may be constrained
    sleep(Duration::from_secs(3)).await;

    let manifest_path = manifest_path("nginx_with_replicas.yml");
    let original_content = tokio::fs::read_to_string(manifest_path.clone())
        .await
        .expect("Failed to read nginx_with_replicas manifest file for verification");

    let task_id = podctl::apply_file(manifest_path.clone(), None)
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

#[cfg(feature = "podman-tests")]
async fn setup_test_environment_for_podman() -> (reqwest::Client, Vec<u16>) {
    setup_cleanup_hook();
    cleanup_key_files(); // Clean up potentially corrupted key files before test
    let _ = env_logger::Builder::from_env(Env::default().default_filter_or("warn")).try_init();

    (reqwest::Client::new(), vec![3000u16, 3100u16, 3200u16])
}

#[cfg(feature = "podman-tests")]
async fn start_test_nodes_for_podman() -> NodeGuard {
    let mut cli1 = make_test_cli(3000, false, true, None, vec![], 4001, false);
    cli1.mock_only_runtime = false;
    cli1.signing_ephemeral = true;
    cli1.kem_ephemeral = true;
    cli1.ephemeral_keys = true;

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
    cli2.signing_ephemeral = true;
    cli2.kem_ephemeral = true;
    cli2.ephemeral_keys = true;

    let bootstrap_peers = vec![
        "/ip4/127.0.0.1/udp/4001/quic-v1".to_string(),
        "/ip4/127.0.0.1/udp/4002/quic-v1".to_string(),
    ];

    let mut cli3 = make_test_cli(3200, false, true, None, bootstrap_peers, 0, false);
    cli3.mock_only_runtime = false;
    cli3.signing_ephemeral = true;
    cli3.kem_ephemeral = true;
    cli3.ephemeral_keys = true;

    start_nodes(vec![cli1, cli2, cli3], Duration::from_secs(1)).await
}

#[cfg(feature = "podman-tests")]
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

#[cfg(feature = "podman-tests")]
#[derive(Debug, Default)]
struct PodmanDeploymentStatus {
    pod_name: Option<String>,
    workload_found: bool,
    sidecar_found: bool,
    sidecar_running: bool,
    sidecar_state: Option<String>,
}

#[cfg(feature = "podman-tests")]
impl PodmanDeploymentStatus {
    fn new() -> Self {
        Self::default()
    }
}

#[cfg(feature = "podman-tests")]
async fn verify_podman_deployment(task_id: &str) -> PodmanDeploymentStatus {
    let mut status = PodmanDeploymentStatus::new();
    let expected_prefix = format!("podmesh-{}", task_id);
    let expected_pod_names = [format!("{}-pod", expected_prefix), expected_prefix.clone()];

    match tokio::process::Command::new("podman")
        .args(["pod", "ls", "--format", "json"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            if let Ok(pods) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                if let Some(pods_array) = pods.as_array() {
                    for pod in pods_array {
                        if let Some(name) = pod.get("Name").and_then(|n| n.as_str()) {
                            if expected_pod_names.iter().any(|expected| expected == name) {
                                status.workload_found = true;
                                status.pod_name = Some(name.to_string());
                                log::info!(
                                    "Found matching Podman pod '{}' for task {}",
                                    name,
                                    task_id
                                );
                                break;
                            }
                        }
                    }
                }
            }
        }
        Ok(output) => {
            log::warn!(
                "'podman pod ls' exited with status {:?} for task {}",
                output.status.code(),
                task_id
            );
        }
        Err(err) => {
            log::warn!("Failed to execute 'podman pod ls': {}", err);
        }
    }

    match tokio::process::Command::new("podman")
        .args(["ps", "-a", "--format", "json"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            if let Ok(containers) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                if let Some(containers_array) = containers.as_array() {
                    for container in containers_array {
                        let container_state = container
                            .get("State")
                            .and_then(|state| state.as_str())
                            .map(|state| state.to_string());

                        if let Some(names) = container.get("Names").and_then(|n| n.as_array()) {
                            for name in names {
                                if let Some(name_str) = name.as_str() {
                                    if name_str.contains(&expected_prefix) {
                                        status.workload_found = true;
                                        if name_str
                                            .to_ascii_lowercase()
                                            .contains(SIDECAR_CONTAINER_NAME)
                                        {
                                            status.sidecar_found = true;
                                            status.sidecar_state = container_state.clone();
                                            if status
                                                .sidecar_state
                                                .as_deref()
                                                .map(|state| state.eq_ignore_ascii_case("running"))
                                                .unwrap_or(false)
                                            {
                                                status.sidecar_running = true;
                                            }
                                            log::info!(
                                                "Found sidecar container '{}' for task {} (state={:?})",
                                                name_str,
                                                task_id,
                                                status.sidecar_state
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(output) => {
            log::warn!(
                "'podman ps -a' exited with status {:?} for task {}",
                output.status.code(),
                task_id
            );
        }
        Err(err) => {
            log::warn!("Failed to execute 'podman ps -a': {}", err);
        }
    }

    status
}

#[cfg(feature = "podman-tests")]
async fn cleanup_podman_resources(task_id: &str) {
    log::info!("Cleaning up Podman resources for task: {}", task_id);

    let expected_pod_name = format!("podmesh-{}-pod", task_id);
    let _ = tokio::process::Command::new("podman")
        .args(["pod", "rm", "-f", &expected_pod_name])
        .output()
        .await;

    let expected_pod_name_alt = format!("podmesh-{}", task_id);
    let _ = tokio::process::Command::new("podman")
        .args(["pod", "rm", "-f", &expected_pod_name_alt])
        .output()
        .await;

    if let Ok(output) = tokio::process::Command::new("podman")
        .args(["pod", "ls", "-q", "--filter", "name=podmesh"])
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
        .args(["ps", "-aq", "--filter", "name=podmesh"])
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
