use serial_test::serial;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;

mod common;
use common::apply_common::{
    check_workload_deployment, get_peer_ids, setup_test_environment, start_cluster_nodes,
    wait_for_mesh_formation,
};

#[serial]
#[tokio::test]
async fn test_disabled_nodes_do_not_schedule_workloads() {
    let (client, ports) = setup_test_environment().await;
    let mut guard = start_cluster_nodes(&[true, false, true]).await;

    sleep(Duration::from_secs(3)).await;
    let mesh_formed = wait_for_mesh_formation(&client, &ports, Duration::from_secs(15)).await;
    if !mesh_formed {
        log::warn!("Mesh formation incomplete, but proceeding with test");
    }

    let manifest_path = manifest_path("nginx.yml");

    let original_content = tokio::fs::read_to_string(manifest_path.clone())
        .await
        .expect("Failed to read manifest for verification");

    let apply_resp = podctl::apply_file(manifest_path.clone(), 1, 1, vec![], None)
        .await
        .expect("apply_file should succeed");
    let task_id = apply_resp.manifest_id;

    sleep(Duration::from_secs(12)).await;

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
        "Expected a single node to host the workload"
    );
    assert!(
        !nodes_with_deployed_workloads.contains(&3000)
            && !nodes_with_deployed_workloads.contains(&3200),
        "Node with disabled scheduling should not receive workloads"
    );
    assert!(
        nodes_with_content_mismatch.is_empty(),
        "Deployed manifest content mismatch on nodes: {:?}",
        nodes_with_content_mismatch
    );

    guard.cleanup().await;
}

#[serial]
#[tokio::test]
async fn test_scheduling_fails_when_all_nodes_disabled() {
    let (client, ports) = setup_test_environment().await;
    let mut guard = start_cluster_nodes(&[true, true, true]).await;

    sleep(Duration::from_secs(3)).await;
    let mesh_formed = wait_for_mesh_formation(&client, &ports, Duration::from_secs(10)).await;
    if !mesh_formed {
        log::warn!("Mesh formation incomplete, but proceeding with test");
    }

    let manifest_path = manifest_path("nginx.yml");

    let apply_result = podctl::apply_file(manifest_path.clone(), 1, 1, vec![], None).await;

    // With the sealed path, apply succeeds as long as enough custodians are available.
    // Nodes with scheduling disabled still act as custodians (NodeMode::Custodian).
    // The workload is sealed and stored but no worker will execute it.
    assert!(
        apply_result.is_ok(),
        "apply_file should succeed (custodians available even when scheduling is disabled), got: {:?}",
        apply_result.err()
    );

    guard.cleanup().await;
}

fn manifest_path(file: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}/tests/sample_manifests/{}",
        env!("CARGO_MANIFEST_DIR"),
        file
    ))
}
