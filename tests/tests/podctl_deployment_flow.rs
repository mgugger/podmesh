//! End-to-end proof of the `podctl` user flow against a real scheduler and real
//! agents.
//!
//! `podctl` is a plain CLI with no Iroh endpoint. It talks HTTP to a scheduler,
//! which relays owner-encrypted bytes to agents over Iroh. Replica spreading is
//! a client decision: `podctl` asks the scheduler for one agent per replica,
//! excluding the agents it already picked, and then admits and deploys against
//! each of them itself. The scheduler never learns how many replicas exist.

use std::{path::PathBuf, sync::Once, time::Duration};

use anyhow::{Context, Result};
use podmesh_integration_tests::{
    mesh::{MESH_TIMEOUT, TestMesh, test_proxy_endpoint},
    support,
};
use serial_test::serial;
use tokio::time::timeout;

/// Matches the container name `podmesh-agent` injects into every workload pod.
const SIDECAR_CONTAINER_NAME: &str = "podmesh-sidecar";
const WORKLOAD_RELAY_TOKEN: &str = "podmesh-test-relay-token-000000000001";
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

static ISOLATE_PODCTL_HOME: Once = Once::new();

/// `podctl` keeps its deployment catalog under `$HOME/.podmesh`. Point that at
/// a throwaway directory so tests never touch the developer's real catalog.
fn isolate_podctl_home() {
    ISOLATE_PODCTL_HOME.call_once(|| {
        let home = tempfile::tempdir().expect("create podctl home");
        // SAFETY: set once, before any test spawns threads that read HOME.
        unsafe { std::env::set_var("HOME", home.path()) };
        std::mem::forget(home);
    });
}

fn manifest(name: &str, replicas: u32) -> Result<(tempfile::TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join(format!("{name}.yaml"));
    std::fs::write(
        &path,
        format!(
            "apiVersion: apps/v1\n\
             kind: Deployment\n\
             metadata:\n  \
               name: {name}\n\
             spec:\n  \
               replicas: {replicas}\n  \
               template:\n    \
                 spec:\n      \
                   containers:\n        \
                     - name: app\n          \
                       image: nginx:alpine\n          \
                       resources:\n            \
                         requests:\n              \
                           cpu: 100m\n              \
                           memory: 64Mi\n",
        ),
    )?;
    Ok((dir, path))
}

async fn apply(path: &PathBuf, api_base: &str) -> Result<String> {
    podctl::apply_file_with_proxy_endpoints(
        path.clone(),
        Some(api_base),
        vec![crypto::b64_encode(
            &test_proxy_endpoint()?.to_bytes(now_secs())?,
        )],
        WORKLOAD_RELAY_TOKEN.to_string(),
        Vec::new(),
    )
    .await
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn setup() {
    support::init_tracing();
    support::init_ephemeral_keys();
    isolate_podctl_home();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn single_replica_deployment_applies_and_deletes() -> Result<()> {
    setup();
    let mesh = timeout(MESH_TIMEOUT, TestMesh::start(1))
        .await
        .context("mesh start timed out")??;
    let (_dir, path) = manifest("single-replica", 1)?;

    let deployment_id = timeout(TEST_TIMEOUT, apply(&path, &mesh.api_base))
        .await
        .context("apply timed out")??;

    let agent = &mesh.agents[0];
    let deployed = agent.runtime.deployed_workload_ids().await;
    assert_eq!(
        deployed.len(),
        1,
        "the single agent must hold exactly one workload"
    );
    assert_eq!(
        mesh.total_deployed_workloads().await,
        1,
        "one replica must produce exactly one workload mesh-wide"
    );

    let status = timeout(
        TEST_TIMEOUT,
        podctl::get_pod(&deployment_id, Some(&mesh.api_base)),
    )
    .await
    .context("status timed out")??;
    assert!(
        status.contains("running"),
        "status must report the running replica, got {status}"
    );

    let deleted = timeout(
        TEST_TIMEOUT,
        podctl::delete_file(path.clone(), false, Some(&mesh.api_base)),
    )
    .await
    .context("delete timed out")??;
    assert_eq!(
        deleted, deployment_id,
        "delete must address the deployment that apply created"
    );
    assert_eq!(
        mesh.total_deployed_workloads().await,
        0,
        "delete must remove the workload from the agent"
    );
    assert!(
        podctl::get_pod(&deployment_id, Some(&mesh.api_base))
            .await
            .is_err(),
        "the local catalog must be gone after a successful delete"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn three_replicas_land_on_three_distinct_agents() -> Result<()> {
    setup();
    let mesh = timeout(MESH_TIMEOUT, TestMesh::start(3))
        .await
        .context("mesh start timed out")??;
    let (_dir, path) = manifest("three-replicas", 3)?;

    let deployment_id = timeout(TEST_TIMEOUT, apply(&path, &mesh.api_base))
        .await
        .context("apply timed out")??;

    assert_eq!(
        mesh.total_deployed_workloads().await,
        3,
        "three replicas must produce three workloads mesh-wide"
    );
    for agent in &mesh.agents {
        assert_eq!(
            agent.runtime.deployed_workload_ids().await.len(),
            1,
            "agent {} must hold exactly one replica; replicas must not share a host",
            agent.endpoint_id
        );
    }

    let mut workload_ids: Vec<String> = Vec::new();
    for agent in &mesh.agents {
        workload_ids.extend(agent.runtime.deployed_workload_ids().await);
    }
    workload_ids.sort();
    workload_ids.dedup();
    assert_eq!(
        workload_ids.len(),
        3,
        "each replica must carry its own workload identity"
    );

    let status = timeout(
        TEST_TIMEOUT,
        podctl::get_pod(&deployment_id, Some(&mesh.api_base)),
    )
    .await
    .context("status timed out")??;
    let reported: Vec<serde_json::Value> = serde_json::from_str(&status)?;
    assert_eq!(reported.len(), 3, "status must report every replica");
    let reported_agents: std::collections::HashSet<&str> = reported
        .iter()
        .filter_map(|entry| entry["agent_endpoint_id"].as_str())
        .collect();
    assert_eq!(
        reported_agents.len(),
        3,
        "the three replicas must be reported from three distinct agents"
    );

    timeout(
        TEST_TIMEOUT,
        podctl::delete_file(path.clone(), false, Some(&mesh.api_base)),
    )
    .await
    .context("delete timed out")??;
    assert_eq!(
        mesh.total_deployed_workloads().await,
        0,
        "delete must remove every replica"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn replica_count_above_available_agents_is_rejected() -> Result<()> {
    setup();
    let mesh = timeout(MESH_TIMEOUT, TestMesh::start(2))
        .await
        .context("mesh start timed out")??;
    let (_dir, path) = manifest("too-many-replicas", 3)?;

    let error = timeout(TEST_TIMEOUT, apply(&path, &mesh.api_base))
        .await
        .context("apply timed out")?
        .expect_err("three replicas must not fit on two agents");
    assert!(
        format!("{error:#}").contains("no agent available for replica 3"),
        "apply must fail loudly on the replica it could not place, got {error:#}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn the_agent_injects_a_sidecar_into_every_replica() -> Result<()> {
    setup();
    let mesh = timeout(MESH_TIMEOUT, TestMesh::start(2))
        .await
        .context("mesh start timed out")??;
    let (_dir, path) = manifest("sidecar-injection", 2)?;

    timeout(TEST_TIMEOUT, apply(&path, &mesh.api_base))
        .await
        .context("apply timed out")??;

    for agent in &mesh.agents {
        let workload_ids = agent.runtime.deployed_workload_ids().await;
        assert_eq!(workload_ids.len(), 1, "agent must hold exactly one replica");
        let manifest = agent
            .runtime
            .deployed_manifest(&workload_ids[0])
            .await
            .context("agent did not record the deployed manifest")?;
        assert_sidecar_injected(&manifest, &workload_ids[0])?;
    }

    timeout(
        TEST_TIMEOUT,
        podctl::delete_file(path.clone(), false, Some(&mesh.api_base)),
    )
    .await
    .context("delete timed out")??;
    Ok(())
}

/// Asserts the agent rewrote the owner's manifest into a pod that carries the
/// podmesh sidecar with everything the sidecar needs to reach a proxy.
fn assert_sidecar_injected(manifest: &[u8], workload_id: &str) -> Result<()> {
    let documents = protocol::manifest_yaml::parse_yaml_documents_from_slice(manifest)?;
    let containers = documents
        .iter()
        .find_map(|document| {
            document
                .get("spec")
                .and_then(|spec| spec.get("template"))
                .and_then(|template| template.get("spec"))
                .or_else(|| document.get("spec"))
                .and_then(|spec| spec.get("containers"))
                .and_then(serde_yaml::Value::as_sequence)
        })
        .context("deployed manifest has no container list")?;

    assert_eq!(
        documents[0]
            .get("spec")
            .and_then(|spec| spec.get("replicas"))
            .and_then(serde_yaml::Value::as_u64),
        Some(1),
        "each agent must run exactly one pod for its replica"
    );

    let sidecar = containers
        .iter()
        .find(|container| {
            container.get("name").and_then(serde_yaml::Value::as_str)
                == Some(SIDECAR_CONTAINER_NAME)
        })
        .context("no podmesh sidecar container was injected")?;

    let env = sidecar
        .get("env")
        .and_then(serde_yaml::Value::as_sequence)
        .context("injected sidecar has no environment")?;
    let env_value = |key: &str| -> Option<&str> {
        env.iter()
            .find(|entry| entry.get("name").and_then(serde_yaml::Value::as_str) == Some(key))
            .and_then(|entry| entry.get("value"))
            .and_then(serde_yaml::Value::as_str)
    };

    let blob = env_value(protocol::sidecar_metadata::METADATA_BLOB_ENV_VAR)
        .context("sidecar metadata blob is missing")?;
    let metadata: protocol::sidecar_metadata::SidecarMetadata =
        serde_json::from_slice(&crypto::b64_decode(blob).context("decode sidecar metadata")?)?;
    assert_eq!(
        metadata.manifest_id, workload_id,
        "sidecar metadata must be bound to the replica it runs beside"
    );
    assert!(
        !metadata.proxy_endpoints.is_empty(),
        "sidecar must be seeded with at least one proxy EndpointRecord for ingress and egress"
    );
    assert_eq!(
        metadata.workload_relay_auth_token, WORKLOAD_RELAY_TOKEN,
        "sidecar must receive the owner's workload relay token"
    );
    assert_eq!(
        env_value("PODMESH_ENABLE_EGRESS"),
        Some("true"),
        "egress must be enabled on the injected sidecar"
    );
    assert!(
        sidecar
            .get("securityContext")
            .and_then(|context| context.get("capabilities"))
            .and_then(|capabilities| capabilities.get("add"))
            .and_then(serde_yaml::Value::as_sequence)
            .is_some_and(|added| added
                .iter()
                .any(|value| value.as_str() == Some("NET_ADMIN"))),
        "the sidecar needs NET_ADMIN to install transparent egress rules"
    );
    Ok(())
}
