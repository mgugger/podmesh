use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use podctl::ClientOptions;
use podmesh_integration_tests::{
    mesh::{MESH_TIMEOUT, TestMesh, test_proxy_endpoint},
    support::{self, ClientKeyDir},
};
use serial_test::serial;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);
const WORKLOAD_RELAY_TOKEN: &str = "podmesh-test-relay-token-000000000001";

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
                           memory: 64Mi\n"
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

fn replace(path: &Path, from: &str, to: &str) -> Result<()> {
    let updated = std::fs::read_to_string(path)?.replace(from, to);
    std::fs::write(path, updated)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn reapply_update_partial_retry_stale_and_scaling_are_safe() -> Result<()> {
    support::init_tracing();
    let mesh = timeout(MESH_TIMEOUT, TestMesh::start(3))
        .await
        .context("mesh start timed out")??;
    let key_dir = ClientKeyDir::with_trusted_agents(&mesh.trusted_agent_keys())?;
    key_dir.activate();
    let options = ClientOptions::with_api_base(Some(&mesh.api_base));
    let (_dir, path) = manifest("update-flow", 3)?;

    let deployment_id = timeout(TEST_TIMEOUT, apply(&path, &options))
        .await
        .context("initial apply timed out")??;
    let initial = podctl::catalog::load(key_dir.path(), &deployment_id)?;
    let placements = initial
        .replicas
        .iter()
        .map(|replica| replica.agent_endpoint_id.clone())
        .collect::<Vec<_>>();
    let initial_calls = mesh
        .agents
        .iter()
        .map(|agent| agent.runtime.deploy_count())
        .collect::<Vec<_>>();

    timeout(TEST_TIMEOUT, apply(&path, &options))
        .await
        .context("unchanged reapply timed out")??;
    ensure!(
        mesh.agents
            .iter()
            .map(|agent| agent.runtime.deploy_count())
            .collect::<Vec<_>>()
            == initial_calls,
        "unchanged reapply redeployed a replica"
    );

    let failing_endpoint = initial.replicas[1].agent_endpoint_id.clone();
    mesh.agent(&failing_endpoint)
        .context("failing agent not found")?
        .runtime
        .fail_next_deploys(1);
    replace(&path, "nginx:alpine", "nginx:1.27")?;
    let partial_error = timeout(TEST_TIMEOUT, apply(&path, &options))
        .await
        .context("partial update timed out")?
        .expect_err("one injected runtime failure must make the update partial");
    ensure!(
        format!("{partial_error:#}").contains("partially successful"),
        "unexpected partial-update error: {partial_error:#}"
    );
    let partial = podctl::catalog::load(key_dir.path(), &deployment_id)?;
    ensure!(
        partial
            .replicas
            .iter()
            .filter(|replica| replica.revision_id != initial.replicas[0].revision_id)
            .count()
            == 2,
        "catalog did not retain exact per-replica partial progress"
    );
    let calls_after_partial = mesh
        .agents
        .iter()
        .map(|agent| agent.runtime.deploy_count())
        .collect::<Vec<_>>();

    timeout(TEST_TIMEOUT, apply(&path, &options))
        .await
        .context("partial retry timed out")??;
    let updated = podctl::catalog::load(key_dir.path(), &deployment_id)?;
    ensure!(
        updated
            .replicas
            .iter()
            .map(|replica| &replica.agent_endpoint_id)
            .eq(placements.iter()),
        "update changed fixed replica placement"
    );
    for agent in &mesh.agents {
        let before = calls_after_partial[mesh
            .agents
            .iter()
            .position(|candidate| candidate.endpoint_id == agent.endpoint_id)
            .unwrap()];
        let expected_increment = usize::from(agent.endpoint_id == failing_endpoint);
        ensure!(
            agent.runtime.deploy_count() == before + expected_increment,
            "retry redeployed a replica that had already reached the requested revision"
        );
    }

    podctl::catalog::save(key_dir.path(), &initial)?;
    replace(&path, "nginx:1.27", "nginx:1.28")?;
    let stale_error = timeout(TEST_TIMEOUT, apply(&path, &options))
        .await
        .context("stale catalog update timed out")?
        .expect_err("stale expected revisions must be refused");
    ensure!(
        format!("{stale_error:#}").contains("partially successful"),
        "unexpected stale-catalog error: {stale_error:#}"
    );

    replace(&path, "replicas: 3", "replicas: 2")?;
    let scaling_error = timeout(TEST_TIMEOUT, apply(&path, &options))
        .await
        .context("scaling refusal timed out")?
        .expect_err("MVP scaling must be refused");
    ensure!(
        format!("{scaling_error:#}").contains("scaling an existing deployment is not supported"),
        "unexpected scaling error: {scaling_error:#}"
    );
    Ok(())
}
