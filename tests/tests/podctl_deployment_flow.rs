//! End-to-end proof of the `podctl` user flow against a real scheduler and real
//! agents.
//!
//! `podctl` is a plain CLI with no Iroh endpoint. It talks HTTP to a scheduler,
//! which relays owner-encrypted bytes to agents over Iroh. Replica spreading is
//! a client decision: `podctl` asks the scheduler for one agent per replica,
//! excluding the agents it already picked, and then admits and deploys against
//! each of them itself. The scheduler never learns how many replicas exist.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use podctl::ClientOptions;
use podmesh_integration_tests::{
    mesh::{MESH_QUERY_TIMEOUT, MESH_TIMEOUT, TestMesh, test_proxy_endpoint},
    support::{self, ClientKeyDir},
};
use serial_test::serial;
use tokio::time::timeout;

/// Matches the container name `podmesh-agent` injects into every workload pod.
const SIDECAR_CONTAINER_NAME: &str = "podmesh-sidecar";
const WORKLOAD_RELAY_TOKEN: &str = "podmesh-test-relay-token-000000000001";
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

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

async fn apply(path: &Path, options: &ClientOptions) -> Result<String> {
    podctl::apply_file_with_proxy_endpoints(
        path.to_path_buf(),
        options,
        vec![crypto::b64_encode(
            &test_proxy_endpoint()?.to_bytes(now_secs())?,
        )],
        WORKLOAD_RELAY_TOKEN.to_string(),
        Vec::new(),
    )
    .await
}

/// Start a mesh and a `podctl` installation that trusts exactly its agents.
///
/// Each agent has its own key directory and therefore its own identity, and the
/// client is told which of them it is prepared to deploy to — the same decision
/// an operator makes with the trusted agent list.
async fn mesh_with_client(agent_count: usize) -> Result<(TestMesh, ClientKeyDir, ClientOptions)> {
    support::init_tracing();
    let mesh = timeout(MESH_TIMEOUT, TestMesh::start(agent_count))
        .await
        .context("mesh start timed out")??;
    let key_dir = ClientKeyDir::with_trusted_agents(&mesh.trusted_agent_keys())?;
    key_dir.activate();
    let options = ClientOptions::with_api_base(Some(&mesh.api_base));
    Ok((mesh, key_dir, options))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A capacity offer is self-signed and names the key the workload is sealed to,
/// so whoever answers a selection request chooses who can read the plaintext.
/// The client must refuse an agent the owner never agreed to before it produces
/// any ciphertext at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn an_agent_outside_the_trust_list_is_refused() -> Result<()> {
    support::init_tracing();
    let mesh = timeout(MESH_TIMEOUT, TestMesh::start(1))
        .await
        .context("mesh start timed out")??;

    // Trust some other agent entirely: the mesh's real agent is not on the list.
    let (stranger, _) = crypto::generate_signing_keypair();
    let key_dir = ClientKeyDir::with_trusted_agents(&[crypto::b64_encode(&stranger)])?;
    key_dir.activate();
    let options = ClientOptions::with_api_base(Some(&mesh.api_base));

    let (_dir, path) = manifest("untrusted-agent", 1)?;
    let error = timeout(TEST_TIMEOUT, apply(&path, &options))
        .await
        .context("apply timed out")?
        .expect_err("deploying to an untrusted agent must fail");
    assert!(
        format!("{error:#}").contains("not in trusted_agents"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        mesh.total_deployed_workloads().await,
        0,
        "nothing may be deployed when the offered agent is not trusted"
    );
    Ok(())
}

/// Refusing to deploy without a trust decision is the default; accepting any
/// agent has to be an explicit act.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn deploying_without_a_trust_decision_is_refused() -> Result<()> {
    support::init_tracing();
    let mesh = timeout(MESH_TIMEOUT, TestMesh::start(1))
        .await
        .context("mesh start timed out")??;
    let key_dir = ClientKeyDir::with_trusted_agents(&[])?;
    key_dir.activate();

    let (_dir, path) = manifest("no-trust-decision", 1)?;
    let options = ClientOptions::with_api_base(Some(&mesh.api_base));
    let error = apply(&path, &options)
        .await
        .expect_err("deploying without a trust decision must fail");
    assert!(
        format!("{error:#}").contains("no trusted agents configured"),
        "unexpected error: {error:#}"
    );

    // The same deployment succeeds once the operator opts out explicitly.
    let options = ClientOptions {
        api_base: Some(mesh.api_base.clone()),
        trust_any_agent: true,
    };
    timeout(TEST_TIMEOUT, apply(&path, &options))
        .await
        .context("apply timed out")??;
    assert_eq!(mesh.total_deployed_workloads().await, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn single_replica_deployment_applies_and_deletes() -> Result<()> {
    let (mesh, _key_dir, options) = mesh_with_client(1).await?;
    let (_dir, path) = manifest("single-replica", 1)?;

    let deployment_id = timeout(TEST_TIMEOUT, apply(&path, &options))
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

    let status = timeout(TEST_TIMEOUT, podctl::get_pod(&deployment_id, &options))
        .await
        .context("status timed out")??;
    assert!(
        status.contains("running"),
        "status must report the running replica, got {status}"
    );

    let deleted = timeout(
        TEST_TIMEOUT,
        podctl::delete_file(path.clone(), false, &options),
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
        podctl::get_pod(&deployment_id, &options).await.is_err(),
        "the local catalog must be gone after a successful delete"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn three_replicas_land_on_three_distinct_agents() -> Result<()> {
    let (mesh, _key_dir, options) = mesh_with_client(3).await?;
    let (_dir, path) = manifest("three-replicas", 3)?;

    let deployment_id = timeout(TEST_TIMEOUT, apply(&path, &options))
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

    let status = timeout(TEST_TIMEOUT, podctl::get_pod(&deployment_id, &options))
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
        podctl::delete_file(path.clone(), false, &options),
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
    let (_mesh, _key_dir, options) = mesh_with_client(2).await?;
    let (_dir, path) = manifest("too-many-replicas", 3)?;

    let error = timeout(TEST_TIMEOUT, apply(&path, &options))
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
    let (mesh, _key_dir, options) = mesh_with_client(2).await?;
    let (_dir, path) = manifest("sidecar-injection", 2)?;

    timeout(TEST_TIMEOUT, apply(&path, &options))
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
        assert_sidecar_injected(&manifest)?;
    }

    timeout(
        TEST_TIMEOUT,
        podctl::delete_file(path.clone(), false, &options),
    )
    .await
    .context("delete timed out")??;
    Ok(())
}

/// Asserts the agent rewrote the owner's manifest into a pod that carries the
/// podmesh sidecar with everything the sidecar needs to reach a proxy.
fn assert_sidecar_injected(manifest: &[u8]) -> Result<()> {
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
    // The routing key is derived from the owner key and the workload name, not
    // from the replica id: every replica of a deployment answers on the same
    // route, and the derivation is what lets a proxy check route ownership.
    assert_eq!(
        metadata.manifest_id,
        protocol::route_id(
            &crypto::b64_decode(&metadata.owner_public_key_b64)?,
            "sidecar-injection"
        ),
        "the routing key must be derived from the owner key and workload name"
    );
    assert_eq!(metadata.workload_name, "sidecar-injection");
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

/// Placement must cost what the mesh actually takes to answer, not the query
/// lifetime.
///
/// A selection that always slept to expiry would make every replica of every
/// deployment pay that timeout in series: a 64-replica apply would spend over
/// five minutes on timers alone, regardless of how fast the agents responded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn placement_returns_as_soon_as_the_mesh_has_answered() -> Result<()> {
    let (mesh, _key_dir, options) = mesh_with_client(3).await?;
    let (_dir, path) = manifest("fast-placement", 3)?;

    let started = std::time::Instant::now();
    timeout(TEST_TIMEOUT, apply(&path, &options))
        .await
        .context("apply timed out")??;
    let elapsed = started.elapsed();

    assert_eq!(mesh.total_deployed_workloads().await, 3);
    // Three replicas are placed in series, so a scheduler that slept out every
    // query would need at least three whole query lifetimes.
    assert!(
        elapsed < MESH_QUERY_TIMEOUT,
        "placing 3 replicas took {elapsed:?}, which is at least one full query \
         lifetime ({MESH_QUERY_TIMEOUT:?}) — the early exit is not working"
    );
    Ok(())
}

/// The local catalog is the only index of where replicas were placed, so a lost
/// or overwritten one leaves workloads running that nothing can address. Asking
/// the mesh is what makes those findable again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn listing_finds_workloads_the_local_catalog_lost() -> Result<()> {
    let (mesh, key_dir, options) = mesh_with_client(2).await?;
    let (_dir, path) = manifest("orphan-hunt", 2)?;

    timeout(TEST_TIMEOUT, apply(&path, &options))
        .await
        .context("apply timed out")??;
    assert_eq!(mesh.total_deployed_workloads().await, 2);

    // Both replicas are known while the catalog is intact.
    let report = timeout(TEST_TIMEOUT, podctl::discover_workloads(&options))
        .await
        .context("list timed out")??;
    assert_eq!(
        report.workloads.len(),
        2,
        "the mesh must report both replicas"
    );
    assert!(
        report.workloads.iter().all(|w| !w.orphaned),
        "nothing is orphaned while the catalog is intact: {report:?}"
    );
    assert!(
        report
            .workloads
            .iter()
            .all(|w| w.workload_name == "orphan-hunt"),
        "the agent must report the workload name so a human can identify it"
    );
    assert!(
        report.unreachable_agents.is_empty(),
        "every agent in this mesh should answer"
    );

    // Simulate the failure this exists for: the catalog is gone, the workloads
    // are not.
    std::fs::remove_dir_all(podctl::catalog::catalog_dir(key_dir.path())?)?;
    assert_eq!(
        podctl::get_pods(podctl::OutputFormat::Json)?.trim(),
        "[]",
        "precondition: the local view is now empty"
    );

    let report = timeout(TEST_TIMEOUT, podctl::discover_workloads(&options))
        .await
        .context("list timed out")??;
    assert_eq!(
        report.workloads.len(),
        2,
        "the mesh still holds the workloads and must still report them"
    );
    assert!(
        report.workloads.iter().all(|w| w.orphaned),
        "workloads the catalog no longer knows about must be flagged as orphaned"
    );
    Ok(())
}
