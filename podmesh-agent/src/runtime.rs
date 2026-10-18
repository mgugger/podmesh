use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{process::Command, sync::RwLock, time::timeout};

mod connector;
mod connector_mock;
mod network;

pub use network::{MANAGED_NETWORK_LABEL, TENANT_NETWORK_LABEL, tenant_network_name};

use protocol::workload_connector::{ConnectorError, ConnectorRequest, ConnectorResponse};

const RUNTIME_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RUNTIME_OUTPUT_BYTES: usize = 1024 * 1024;
/// Upper bound on the containers whose logs a single request may collect.
/// A workload pod holds an application container plus the injected sidecar;
/// the bound keeps a hostile manifest from turning one request into hundreds
/// of runtime invocations.
const MAX_LOGGED_CONTAINERS: usize = 16;

#[async_trait]
pub trait WorkloadRuntime: Send + Sync {
    fn target_id(&self, deployment: &WorkloadDeployment<'_>) -> Result<String>;
    async fn deploy(&self, deployment: WorkloadDeployment<'_>) -> Result<String>;
    async fn status(&self, runtime_id: &str) -> Result<String>;
    async fn logs(&self, runtime_id: &str, tail: u32) -> Result<String>;
    async fn delete(&self, runtime_id: &str) -> Result<()>;
    /// Transport one bounded request to the connector INSIDE this workload.
    /// Backends may use container exec or a guest-local channel. This transport
    /// is not an identity issuer and never grants machine-plane membership.
    async fn connector_exchange(
        &self,
        _runtime_id: &str,
        request: ConnectorRequest,
    ) -> Result<ConnectorResponse> {
        request.validate()?;
        Ok(ConnectorResponse::error(ConnectorError::Unsupported))
    }
    async fn cleanup_failed_deploy(&self, runtime_id: &str, uncertain: bool) -> Result<()> {
        anyhow::ensure!(
            !uncertain,
            "runtime creation outcome is uncertain; operator repair required"
        );
        self.delete(runtime_id).await
    }
}

#[derive(Debug)]
pub struct RuntimeOutcomeUnknown;

impl std::fmt::Display for RuntimeOutcomeUnknown {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("runtime operation timed out or lost completion; outcome uncertain")
    }
}

impl std::error::Error for RuntimeOutcomeUnknown {}

/// One pod to run, and the tenant it belongs to.
pub struct WorkloadDeployment<'a> {
    pub workload_id: &'a str,
    /// Base64 Ed25519 key of the namespace owner, recorded so a deployment can
    /// be attributed to its tenant.
    pub namespace_id: &'a str,
    pub manifest: &'a [u8],
}

#[derive(Default)]
pub struct MockRuntime {
    workloads: RwLock<HashMap<String, Vec<u8>>>,
    /// Namespace each workload was deployed for, so tests can assert that pods
    /// are placed per tenant.
    tenants: RwLock<HashMap<String, String>>,
    connector_responses: RwLock<HashMap<String, ConnectorResponse>>,
    connector_nodes: RwLock<HashMap<String, connector_mock::MockConnector>>,
    deploy_calls: AtomicUsize,
    fail_deploys: AtomicUsize,
    status_delay_millis: AtomicUsize,
    active_status_calls: AtomicUsize,
    max_active_status_calls: AtomicUsize,
}

impl MockRuntime {
    /// Manifest the agent handed to the runtime, i.e. after sidecar injection.
    pub async fn deployed_manifest(&self, workload_id: &str) -> Option<Vec<u8>> {
        self.workloads.read().await.get(workload_id).cloned()
    }

    /// Namespace a workload was deployed for.
    pub async fn deployed_namespace(&self, workload_id: &str) -> Option<String> {
        self.tenants.read().await.get(workload_id).cloned()
    }

    /// Workload IDs currently held by this runtime.
    pub async fn deployed_workload_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.workloads.read().await.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Explicit mock response; reports are still verified by the agent service.
    pub async fn set_connector_response(
        &self,
        runtime_id: &str,
        response: ConnectorResponse,
    ) -> Result<()> {
        response.to_bytes()?;
        self.connector_responses
            .write()
            .await
            .insert(runtime_id.to_owned(), response);
        Ok(())
    }

    pub fn deploy_count(&self) -> usize {
        self.deploy_calls.load(Ordering::SeqCst)
    }

    pub fn fail_next_deploys(&self, count: usize) {
        self.fail_deploys.store(count, Ordering::SeqCst);
    }

    pub fn set_status_delay(&self, delay: Duration) {
        self.status_delay_millis.store(
            usize::try_from(delay.as_millis()).unwrap_or(usize::MAX),
            Ordering::SeqCst,
        );
    }

    pub fn max_concurrent_status_calls(&self) -> usize {
        self.max_active_status_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl WorkloadRuntime for MockRuntime {
    fn target_id(&self, deployment: &WorkloadDeployment<'_>) -> Result<String> {
        Ok(deployment.workload_id.to_string())
    }

    async fn cleanup_failed_deploy(&self, runtime_id: &str, _uncertain: bool) -> Result<()> {
        self.delete(runtime_id).await
    }

    async fn deploy(&self, deployment: WorkloadDeployment<'_>) -> Result<String> {
        self.deploy_calls.fetch_add(1, Ordering::SeqCst);
        loop {
            let current = self.fail_deploys.load(Ordering::SeqCst);
            let Some(next) = current.checked_sub(1) else {
                break;
            };
            if self
                .fail_deploys
                .compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Err(anyhow!("injected mock runtime deploy failure"));
            }
        }
        self.workloads.write().await.insert(
            deployment.workload_id.to_string(),
            deployment.manifest.to_vec(),
        );
        self.tenants.write().await.insert(
            deployment.workload_id.to_string(),
            deployment.namespace_id.to_string(),
        );
        if deployment
            .manifest
            .windows(b"PODMESH_SIDECAR_METADATA_B64".len())
            .any(|bytes| bytes == b"PODMESH_SIDECAR_METADATA_B64")
        {
            // Exercise production enrollment/bootstrap in ordinary owner-flow
            // tests rather than bypassing it with unsupported mock responses.
            self.enable_connector(deployment.workload_id).await?;
            let mut nodes = self.connector_nodes.write().await;
            if nodes
                .get(deployment.workload_id)
                .is_some_and(|node| !node.network_configured())
            {
                nodes.remove(deployment.workload_id);
            }
        }
        Ok(deployment.workload_id.to_string())
    }

    async fn status(&self, runtime_id: &str) -> Result<String> {
        let active = self.active_status_calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active_status_calls
            .fetch_max(active, Ordering::SeqCst);
        let delay = self.status_delay_millis.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(
                u64::try_from(delay).unwrap_or(u64::MAX),
            ))
            .await;
        }
        let exists = self.workloads.read().await.contains_key(runtime_id);
        self.active_status_calls.fetch_sub(1, Ordering::SeqCst);
        if exists {
            Ok("running".into())
        } else {
            Err(anyhow!("workload not found"))
        }
    }

    async fn logs(&self, runtime_id: &str, _tail: u32) -> Result<String> {
        self.status(runtime_id).await?;
        Ok(String::new())
    }

    async fn connector_exchange(
        &self,
        runtime_id: &str,
        request: ConnectorRequest,
    ) -> Result<ConnectorResponse> {
        request.validate()?;
        anyhow::ensure!(
            self.workloads.read().await.contains_key(runtime_id),
            "workload not found"
        );
        if let Some(response) = self
            .connector_responses
            .read()
            .await
            .get(runtime_id)
            .cloned()
        {
            return Ok(response);
        }
        Ok(self
            .mock_connector_exchange(runtime_id, request)
            .await?
            .unwrap_or_else(|| ConnectorResponse::error(ConnectorError::Unsupported)))
    }

    async fn delete(&self, runtime_id: &str) -> Result<()> {
        self.workloads.write().await.remove(runtime_id);
        self.connector_responses.write().await.remove(runtime_id);
        self.connector_nodes.write().await.remove(runtime_id);
        Ok(())
    }
}

pub struct PodmanRuntime {
    network_prefix: String,
}

impl PodmanRuntime {
    pub fn new(network_prefix: &str) -> Result<Self> {
        network::validate_prefix(network_prefix)?;
        Ok(Self {
            network_prefix: network_prefix.to_string(),
        })
    }

    async fn output(command: &mut Command) -> Result<String> {
        command.kill_on_drop(true);
        let output = timeout(RUNTIME_COMMAND_TIMEOUT, command.output())
            .await
            .map_err(|_| anyhow!(RuntimeOutcomeUnknown))?
            .map_err(|error| anyhow!(error).context(RuntimeOutcomeUnknown))?;
        let mut combined = output.stdout;
        combined.extend_from_slice(&output.stderr);
        anyhow::ensure!(
            combined.len() <= MAX_RUNTIME_OUTPUT_BYTES,
            "runtime output exceeds limit"
        );
        let text = String::from_utf8_lossy(&combined).into_owned();
        if !output.status.success() {
            return Err(anyhow!("runtime command failed: {text}"));
        }
        Ok(text)
    }

    fn pod_name(manifest: &[u8]) -> Result<String> {
        let documents = protocol::manifest_yaml::parse_yaml_documents_from_slice(manifest)
            .context("parse workload manifest")?;
        let value = documents
            .iter()
            // The same predicate sidecar injection uses. A looser one here
            // would let a manifest whose first document is a spec-less `Pod`
            // name any pod on the host, including another tenant's.
            .find(|document| {
                let is_pod =
                    document.get("kind").and_then(serde_yaml::Value::as_str) == Some("Pod");
                if is_pod {
                    document.get("spec").is_some()
                } else {
                    document
                        .get("spec")
                        .and_then(|spec| spec.get("template"))
                        .and_then(|template| template.get("spec"))
                        .is_some()
                }
            })
            .ok_or_else(|| anyhow!("manifest does not contain a pod workload"))?;
        let name = value
            .get("metadata")
            .and_then(|metadata| metadata.get("name"))
            .and_then(|name| name.as_str())
            .ok_or_else(|| anyhow!("workload metadata.name is required"))?;
        anyhow::ensure!(
            name.len() <= 253
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.')),
            "invalid workload name"
        );
        if value.get("kind").and_then(serde_yaml::Value::as_str) == Some("Pod") {
            Ok(name.to_string())
        } else {
            Ok(format!("{name}-pod"))
        }
    }
}

#[async_trait]
impl WorkloadRuntime for PodmanRuntime {
    async fn connector_exchange(
        &self,
        runtime_id: &str,
        request: ConnectorRequest,
    ) -> Result<ConnectorResponse> {
        self.exchange_connector(runtime_id, request).await
    }

    fn target_id(&self, deployment: &WorkloadDeployment<'_>) -> Result<String> {
        Self::pod_name(deployment.manifest)
    }

    async fn deploy(&self, deployment: WorkloadDeployment<'_>) -> Result<String> {
        let pod_name = Self::pod_name(deployment.manifest)?;
        let mut file = tempfile::NamedTempFile::new().context("create protected manifest file")?;
        file.as_file_mut()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        std::io::Write::write_all(&mut file, deployment.manifest)?;
        let network = self.ensure_tenant_network(deployment.namespace_id).await?;
        Self::output(
            Command::new("podman")
                .arg("kube")
                .arg("play")
                .arg("--network")
                .arg(&network)
                .arg("--replace")
                .arg(file.path()),
        )
        .await?;
        Ok(pod_name)
    }

    async fn status(&self, runtime_id: &str) -> Result<String> {
        Self::output(
            Command::new("podman")
                .arg("pod")
                .arg("inspect")
                .arg("--format")
                .arg("{{.State}}")
                .arg(runtime_id),
        )
        .await
    }

    /// Collects logs container by container.
    ///
    /// `podman pod logs` is not served by the remote socket the agent talks
    /// to, so the pod is expanded into its containers first and each one is
    /// read on its own.
    async fn logs(&self, runtime_id: &str, tail: u32) -> Result<String> {
        let listed = Self::output(
            Command::new("podman")
                .arg("pod")
                .arg("inspect")
                .arg("--format")
                .arg("{{range .Containers}}{{.Name}}\n{{end}}")
                .arg(runtime_id),
        )
        .await
        .context("list workload pod containers")?;
        let containers: Vec<&str> = listed
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .take(MAX_LOGGED_CONTAINERS)
            .collect();
        anyhow::ensure!(!containers.is_empty(), "workload pod has no containers");
        let mut collected = String::new();
        for container in containers {
            let output = Self::output(
                Command::new("podman")
                    .arg("logs")
                    .arg("--tail")
                    .arg(tail.to_string())
                    .arg(container),
            )
            .await
            .with_context(|| format!("read logs of container {container}"))?;
            collected.push_str("==> ");
            collected.push_str(container);
            collected.push_str(" <==\n");
            collected.push_str(&output);
            if !output.ends_with('\n') {
                collected.push('\n');
            }
            anyhow::ensure!(
                collected.len() <= MAX_RUNTIME_OUTPUT_BYTES,
                "workload logs exceed limit"
            );
        }
        Ok(collected)
    }

    async fn delete(&self, runtime_id: &str) -> Result<()> {
        Self::output(
            Command::new("podman")
                .arg("pod")
                .arg("rm")
                .arg("--force")
                .arg("--ignore")
                .arg(runtime_id),
        )
        .await?;
        Ok(())
    }
}

pub fn create_runtime(
    kind: crate::config::RuntimeKind,
    network_prefix: &str,
) -> Result<Arc<dyn WorkloadRuntime>> {
    match kind {
        crate::config::RuntimeKind::Podman => Ok(Arc::new(PodmanRuntime::new(network_prefix)?)),
        crate::config::RuntimeKind::Mock => Ok(Arc::new(MockRuntime::default())),
    }
}

#[cfg(test)]
mod pending_cleanup_tests {
    use super::*;

    #[tokio::test]
    async fn podman_refuses_to_infer_cleanup_from_an_interrupted_request() {
        let runtime = PodmanRuntime::new("podmesh").unwrap();
        let error = runtime
            .cleanup_failed_deploy("must-not-execute-podman", true)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("operator repair required"));
    }

    #[test]
    fn podman_resolves_pod_and_deployment_targets_before_execution() {
        let runtime = PodmanRuntime::new("podmesh").unwrap();
        for (manifest, expected) in [
            ("kind: Pod\nmetadata: {name: owned}\nspec: {}", "owned"),
            (
                "kind: Deployment\nmetadata: {name: owned}\nspec: {template: {spec: {}}}",
                "owned-pod",
            ),
        ] {
            assert_eq!(
                runtime
                    .target_id(&WorkloadDeployment {
                        workload_id: "id",
                        namespace_id: "owner",
                        manifest: manifest.as_bytes()
                    })
                    .unwrap(),
                expected
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_workload_network_prefix() {
        assert!(PodmanRuntime::new("podmesh").is_ok());
        assert!(PodmanRuntime::new("mesh.test_1").is_ok());
        assert!(PodmanRuntime::new("").is_err());
        assert!(PodmanRuntime::new("mesh network").is_err());
        assert!(PodmanRuntime::new(&"a".repeat(64)).is_err());
    }
}
