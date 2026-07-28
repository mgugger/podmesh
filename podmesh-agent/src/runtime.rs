use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::{collections::HashMap, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use tokio::{process::Command, sync::RwLock, time::timeout};

const RUNTIME_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RUNTIME_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_WORKLOAD_NETWORK_LEN: usize = 128;
/// Upper bound on the containers whose logs a single request may collect.
/// A workload pod holds an application container plus the injected sidecar;
/// the bound keeps a hostile manifest from turning one request into hundreds
/// of runtime invocations.
const MAX_LOGGED_CONTAINERS: usize = 16;

#[async_trait]
pub trait WorkloadRuntime: Send + Sync {
    async fn deploy(&self, workload_id: &str, manifest: &[u8]) -> Result<String>;
    async fn status(&self, runtime_id: &str) -> Result<String>;
    async fn logs(&self, runtime_id: &str, tail: u32) -> Result<String>;
    async fn delete(&self, runtime_id: &str) -> Result<()>;
}

#[derive(Default)]
pub struct MockRuntime {
    workloads: RwLock<HashMap<String, Vec<u8>>>,
}

impl MockRuntime {
    /// Manifest the agent handed to the runtime, i.e. after sidecar injection.
    pub async fn deployed_manifest(&self, workload_id: &str) -> Option<Vec<u8>> {
        self.workloads.read().await.get(workload_id).cloned()
    }

    /// Workload IDs currently held by this runtime.
    pub async fn deployed_workload_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.workloads.read().await.keys().cloned().collect();
        ids.sort();
        ids
    }
}

#[async_trait]
impl WorkloadRuntime for MockRuntime {
    async fn deploy(&self, workload_id: &str, manifest: &[u8]) -> Result<String> {
        self.workloads
            .write()
            .await
            .insert(workload_id.to_string(), manifest.to_vec());
        Ok(workload_id.to_string())
    }

    async fn status(&self, runtime_id: &str) -> Result<String> {
        if self.workloads.read().await.contains_key(runtime_id) {
            Ok("running".into())
        } else {
            Err(anyhow!("workload not found"))
        }
    }

    async fn logs(&self, runtime_id: &str, _tail: u32) -> Result<String> {
        self.status(runtime_id).await?;
        Ok(String::new())
    }

    async fn delete(&self, runtime_id: &str) -> Result<()> {
        self.workloads.write().await.remove(runtime_id);
        Ok(())
    }
}

pub struct PodmanRuntime {
    network: String,
}

impl PodmanRuntime {
    pub fn new(network: &str) -> Result<Self> {
        anyhow::ensure!(
            !network.is_empty()
                && network.len() <= MAX_WORKLOAD_NETWORK_LEN
                && network.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                }),
            "invalid workload network"
        );
        Ok(Self {
            network: network.to_string(),
        })
    }

    async fn output(command: &mut Command) -> Result<String> {
        let output = timeout(RUNTIME_COMMAND_TIMEOUT, command.output())
            .await
            .map_err(|_| anyhow!("runtime command timed out"))??;
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
            .find(|document| {
                document.get("kind").and_then(serde_yaml::Value::as_str) == Some("Pod")
                    || document
                        .get("spec")
                        .and_then(|spec| spec.get("template"))
                        .and_then(|template| template.get("spec"))
                        .is_some()
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
    async fn deploy(&self, _workload_id: &str, manifest: &[u8]) -> Result<String> {
        let pod_name = Self::pod_name(manifest)?;
        let mut file = tempfile::NamedTempFile::new().context("create protected manifest file")?;
        file.as_file_mut()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        std::io::Write::write_all(&mut file, manifest)?;
        Self::output(
            Command::new("podman")
                .arg("kube")
                .arg("play")
                .arg("--network")
                .arg(&self.network)
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
    workload_network: &str,
) -> Result<Arc<dyn WorkloadRuntime>> {
    match kind {
        crate::config::RuntimeKind::Podman => Ok(Arc::new(PodmanRuntime::new(workload_network)?)),
        crate::config::RuntimeKind::Mock => Ok(Arc::new(MockRuntime::default())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_workload_network_name() {
        assert!(PodmanRuntime::new("podmesh").is_ok());
        assert!(PodmanRuntime::new("mesh.test_1").is_ok());
        assert!(PodmanRuntime::new("").is_err());
        assert!(PodmanRuntime::new("mesh network").is_err());
        assert!(PodmanRuntime::new(&"a".repeat(MAX_WORKLOAD_NETWORK_LEN + 1)).is_err());
    }
}
