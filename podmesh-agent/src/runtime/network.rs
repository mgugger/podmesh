//! Tenant-scoped Podman networks. No infrastructure is attached here: operators
//! supply reachable mesh endpoints; local tests arrange their own connectivity.
use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use tokio::process::Command;

use super::PodmanRuntime;

pub const MANAGED_NETWORK_LABEL: &str = "io.podmesh.managed";
pub const TENANT_NETWORK_LABEL: &str = "io.podmesh.tenant";
const MAX_NETWORK_PREFIX_LEN: usize = 63;

pub(super) fn validate_prefix(prefix: &str) -> Result<()> {
    ensure!(
        !prefix.is_empty()
            && prefix.len() <= MAX_NETWORK_PREFIX_LEN
            && prefix.as_bytes()[0].is_ascii_alphanumeric()
            && prefix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "invalid workload network prefix"
    );
    Ok(())
}

fn tenant_digest(namespace_id: &str) -> Result<String> {
    let owner = crypto::b64_decode(namespace_id).context("decode network tenant identity")?;
    ensure!(
        owner.len() == crypto::ED25519_PUBLIC_KEY_SIZE,
        "invalid network tenant identity length"
    );
    Ok(blake3::hash(&owner).to_hex().to_string())
}

/// Stable within a Podman instance, independent of agent, workload or replica.
pub fn tenant_network_name(prefix: &str, namespace_id: &str) -> Result<String> {
    validate_prefix(prefix)?;
    Ok(format!("{prefix}-{}", tenant_digest(namespace_id)?))
}

#[derive(Deserialize)]
struct Network {
    name: String,
    driver: String,
    dns_enabled: bool,
    internal: bool,
    #[serde(default)]
    labels: HashMap<String, String>,
}

fn validate_network(bytes: &str, name: &str, tenant: &str) -> Result<()> {
    let networks: Vec<Network> = serde_json::from_str(bytes).context("inspect tenant network")?;
    ensure!(networks.len() == 1, "expected exactly one tenant network");
    let network = &networks[0];
    ensure!(
        network.name == name
            && network.driver == "bridge"
            && network.dns_enabled
            && !network.internal
            && network
                .labels
                .get(MANAGED_NETWORK_LABEL)
                .map(String::as_str)
                == Some("true")
            && network.labels.get(TENANT_NETWORK_LABEL).map(String::as_str) == Some(tenant),
        "tenant network ownership or configuration mismatch: {name}"
    );
    Ok(())
}

impl PodmanRuntime {
    /// Idempotent across processes sharing a Podman socket. Never remove a
    /// network here: another agent may have workloads attached to it.
    pub async fn ensure_tenant_network(&self, namespace_id: &str) -> Result<String> {
        let name = tenant_network_name(&self.network_prefix, namespace_id)?;
        let tenant = tenant_digest(namespace_id)?;
        Self::output(Command::new("podman").args([
            "network",
            "create",
            "--ignore",
            "--driver",
            "bridge",
            "--label",
            &format!("{MANAGED_NETWORK_LABEL}=true"),
            "--label",
            &format!("{TENANT_NETWORK_LABEL}={tenant}"),
            &name,
        ]))
        .await
        .context("create tenant network")?;
        // --ignore accepts any existing network. Refuse name collisions rather
        // than silently trusting an unrelated/preconfigured bridge.
        let inspected = Self::output(Command::new("podman").args(["network", "inspect", &name]))
            .await
            .context("inspect tenant network")?;
        validate_network(&inspected, &name, &tenant)?;
        Ok(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn validates_network_prefix_and_owner() {
        for prefix in ["", "-option", "space here", "path/name", &"a".repeat(64)] {
            assert!(validate_prefix(prefix).is_err());
        }
        assert!(tenant_network_name("podmesh-tenant", "not base64").is_err());
        assert!(tenant_network_name("podmesh-tenant", &crypto::b64_encode(&[1; 31])).is_err());
    }

    #[test]
    fn refuses_unmanaged_or_misconfigured_networks() {
        let mut network = serde_json::json!([{
            "name": "tenant", "driver": "bridge", "dns_enabled": true, "internal": false,
            "labels": {MANAGED_NETWORK_LABEL: "true", TENANT_NETWORK_LABEL: "owner"}
        }]);
        validate_network(&network.to_string(), "tenant", "owner").unwrap();
        assert!(validate_network(&network.to_string(), "tenant", "other").is_err());
        for (field, bad) in [
            ("driver", serde_json::json!("macvlan")),
            ("dns_enabled", serde_json::json!(false)),
            ("internal", serde_json::json!(true)),
            ("labels", serde_json::json!({})),
        ] {
            let original = network[0][field].clone();
            network[0][field] = bad;
            assert!(validate_network(&network.to_string(), "tenant", "owner").is_err());
            network[0][field] = original;
        }
    }

    proptest! {
        #[test]
        fn network_names_are_stable_and_owner_scoped(owner in any::<[u8; 32]>(), other in any::<[u8; 32]>()) {
            let name = tenant_network_name("podmesh-tenant", &crypto::b64_encode(&owner)).unwrap();
            prop_assert_eq!(&name, &tenant_network_name("podmesh-tenant", &crypto::b64_encode(&owner)).unwrap());
            prop_assert_eq!(name.len(), "podmesh-tenant".len() + 1 + 64);
            if owner != other {
                prop_assert_ne!(name, tenant_network_name("podmesh-tenant", &crypto::b64_encode(&other)).unwrap());
            }
        }
    }
}
