//! Test-only connectivity. Production creates tenant bridges but never attaches
//! infrastructure to them. These fixtures retain the local stack's DNS names.
use super::*;
use podmesh_agent::{
    config::DEFAULT_WORKLOAD_NETWORK_PREFIX,
    runtime::{PodmanRuntime, tenant_network_name},
};

pub struct TenantNetworkGuard {
    pub name: String,
    attachments: Vec<String>,
}

impl TenantNetworkGuard {
    /// The fresh test owner's network is fixture-owned. Declare this guard
    /// before workload guards so their pods disappear before network cleanup.
    pub async fn prepare(owner: &[u8], infrastructure_pods: &[&str]) -> Result<Self> {
        let namespace = crypto::b64_encode(owner);
        let name = tenant_network_name(DEFAULT_WORKLOAD_NETWORK_PREFIX, &namespace)?;
        anyhow::ensure!(
            !podman_status(&["network", "exists", &name]).await?,
            "test tenant network already exists: {name}"
        );
        PodmanRuntime::new(DEFAULT_WORKLOAD_NETWORK_PREFIX)?
            .ensure_tenant_network(&namespace)
            .await?;
        // Take cleanup ownership only after the runtime has verified labels
        // and configuration; a conflicting pre-existing network is not ours.
        let mut guard = Self {
            name,
            attachments: Vec::new(),
        };
        for pod in infrastructure_pods {
            let infra =
                podman_output(&["pod", "inspect", "--format", "{{.InfraContainerID}}", pod])
                    .await?;
            let infra = infra.trim().to_string();
            anyhow::ensure!(
                !infra.is_empty(),
                "test infrastructure pod has no infra container"
            );
            guard.attachments.push(infra.clone());
            podman_output(&["network", "connect", "--alias", pod, &guard.name, &infra]).await?;
        }
        if infrastructure_pods.contains(&"podmesh-proxies") {
            // Iroh records captured before live interface attachment can still
            // advertise only the old bridge. Rebind the disposable fixture
            // before callers mint grants (which are intentionally in-memory).
            podman_output(&["pod", "restart", "podmesh-proxies"]).await?;
            let index = infrastructure_pods
                .iter()
                .position(|pod| *pod == "podmesh-proxies")
                .context("proxy sandbox missing")?;
            let infra = &guard.attachments[index];
            let inspected: serde_json::Value =
                serde_json::from_str(&podman_output(&["inspect", infra]).await?)?;
            let proxy_ip: std::net::IpAddr = inspected[0]["NetworkSettings"]["Networks"]
                [&guard.name]["IPAddress"]
                .as_str()
                .context("proxy tenant address missing")?
                .parse()?;
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()?;
            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                for port in [3010, 3011, 3012] {
                    loop {
                        if let Ok(response) = client
                            .get(format!("http://127.0.0.1:{port}/api/v1/endpoint_record"))
                            .send()
                            .await
                            && response.status().is_success()
                            && let Ok(body) = response.json::<serde_json::Value>().await
                            && let Some(encoded) = body["endpoint_record_b64"].as_str()
                            && let Ok(bytes) = crypto::b64_decode(encoded)
                            && let Ok(now) =
                                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                            && let Ok(record) =
                                protocol::EndpointRecord::from_bytes(&bytes, now.as_secs())
                            && record.direct_addresses.iter().any(|address| {
                                address
                                    .parse::<std::net::SocketAddr>()
                                    .is_ok_and(|address| address.ip() == proxy_ip)
                            })
                        {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            })
            .await
            .context("test proxies did not recover after network attachment")?;
        }
        Ok(guard)
    }
}

impl Drop for TenantNetworkGuard {
    fn drop(&mut self) {
        for infra in &self.attachments {
            let _ = std::process::Command::new("podman")
                .args(["network", "disconnect", &self.name, infra])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        // Never --force: an unexpected live workload must not be destroyed by
        // fixture cleanup. Report leaks instead of hiding them.
        match std::process::Command::new("podman")
            .args(["network", "rm", &self.name])
            .output()
        {
            Ok(output) if output.status.success() => {}
            result => log::warn!(
                "could not remove test tenant network {}: {result:?}",
                self.name
            ),
        }
    }
}
