//! Real Podman runtime contract; deliberately no proxy/control attachments.
#![cfg(feature = "podman-tests")]

use anyhow::{Context, Result, ensure};
use podmesh_agent::runtime::{
    PodmanRuntime, WorkloadDeployment, WorkloadRuntime, tenant_network_name,
};
use podmesh_integration_tests::support::init_tracing;
use serde_json::Value;
use std::process::{Command as StdCommand, Stdio};
use tokio::process::Command;

async fn podman(args: &[&str]) -> Result<String> {
    let output = Command::new("podman")
        .args(args)
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "podman {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[derive(Default)]
struct Resources {
    pods: Vec<String>,
    networks: Vec<String>,
}
impl Drop for Resources {
    fn drop(&mut self) {
        for pod in &self.pods {
            let _ = StdCommand::new("podman")
                .args(["pod", "rm", "--force", pod])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        for network in &self.networks {
            let _ = StdCommand::new("podman")
                .args(["network", "rm", network])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn deploy_creates_and_reuses_only_the_owners_network() -> Result<()> {
    init_tracing();
    podman(&["info"])
        .await
        .context("podman-tests requires a working Podman installation")?;
    podman(&["image", "exists", "docker.io/library/nginx:latest"])
        .await
        .context("preload docker.io/library/nginx:latest for podman-tests")?;
    let prefix = format!("pmt-{}", crypto::generate_secure_nonce());
    let owner = crypto::b64_encode(&crypto::generate_signing_keypair().0);
    let other = crypto::b64_encode(&crypto::generate_signing_keypair().0);
    let conflict_owner = crypto::b64_encode(&crypto::generate_signing_keypair().0);
    let mut resources = Resources::default();
    let first_network = tenant_network_name(&prefix, &owner)?;
    let second_network = tenant_network_name(&prefix, &other)?;
    let conflict_network = tenant_network_name(&prefix, &conflict_owner)?;
    resources.networks = vec![
        first_network.clone(),
        second_network.clone(),
        conflict_network.clone(),
    ];
    let names = [
        format!("{prefix}-a"),
        format!("{prefix}-b"),
        format!("{prefix}-c"),
    ];
    resources.pods = names.to_vec();
    let manifest = |name: &str| {
        format!(
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: {name}\nspec:\n  containers:\n    - name: app\n      image: docker.io/library/nginx:latest\n      imagePullPolicy: Never\n"
        )
    };
    let a = manifest(&names[0]);
    let b = manifest(&names[1]);
    let c = manifest(&names[2]);
    let runtime = PodmanRuntime::new(&prefix)?;
    let sibling = PodmanRuntime::new(&prefix)?;
    // Construction/admission alone must not allocate a network.
    let absent = Command::new("podman")
        .args(["network", "exists", &first_network])
        .status()
        .await?;
    ensure!(
        !absent.success(),
        "network unexpectedly exists before deployment"
    );
    let deployment = |id, namespace, manifest| WorkloadDeployment {
        workload_id: id,
        namespace_id: namespace,
        manifest,
    };
    let (a_result, b_result) = tokio::join!(
        runtime.deploy(deployment(&names[0], &owner, a.as_bytes())),
        sibling.deploy(deployment(&names[1], &owner, b.as_bytes())),
    );
    a_result?;
    b_result?;
    runtime
        .deploy(deployment(&names[2], &other, c.as_bytes()))
        .await?;
    ensure!(first_network != second_network);
    for (pod, network) in [
        (&names[0], &first_network),
        (&names[1], &first_network),
        (&names[2], &second_network),
    ] {
        let infra = podman(&["pod", "inspect", "--format", "{{.InfraContainerID}}", pod]).await?;
        let inspected: Value = serde_json::from_str(&podman(&["inspect", infra.trim()]).await?)?;
        let networks = inspected[0]["NetworkSettings"]["Networks"]
            .as_object()
            .context("pod has no networks")?;
        ensure!(
            networks.len() == 1 && networks.contains_key(network),
            "unexpected networks for {pod}: {networks:?}"
        );
        runtime.status(pod).await?;
        runtime.logs(pod, 2).await?;
    }
    // A replacement uses the same network. Deleting one pod retains both the
    // shared tenant network and its sibling pod.
    runtime
        .deploy(deployment(&names[0], &owner, a.as_bytes()))
        .await?;
    runtime.delete(&names[0]).await?;
    runtime.status(&names[1]).await?;
    podman(&["network", "inspect", &first_network]).await?;
    ensure!(sibling.ensure_tenant_network(&owner).await? == first_network);
    runtime.delete(&names[1]).await?;
    runtime.delete(&names[2]).await?;
    podman(&["network", "inspect", &first_network]).await?;
    podman(&["network", "inspect", &second_network]).await?;
    podman(&["network", "create", &conflict_network]).await?;
    let error = runtime
        .ensure_tenant_network(&conflict_owner)
        .await
        .unwrap_err();
    ensure!(
        error
            .to_string()
            .contains("ownership or configuration mismatch"),
        "unexpected collision error: {error:#}"
    );
    Ok(())
}
