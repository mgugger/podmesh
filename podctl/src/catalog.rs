//! Local record of where a deployment's replicas live.
//!
//! `podctl` places replicas itself and the mesh keeps no owner-side index, so
//! this catalog is the only handle on a running deployment. It is written
//! incrementally — one entry per replica, as soon as that replica is confirmed —
//! because a deployment that fails on its third replica must still leave the
//! first two addressable and deletable.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

use protocol::{DeploymentReceipt, EndpointRecord};

pub const DEPLOYMENT_CATALOG_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaUpdateState {
    Active,
    Updating { requested_revision_id: String },
}

/// One replica of a deployment, pinned to the agent `podctl` selected for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaPlacement {
    pub replica_index: u32,
    pub revision_id: String,
    pub receipt: DeploymentReceipt,
    /// Scheduler HTTP endpoint that relayed this replica. Lifecycle commands
    /// go back through a scheduler, never straight to the agent.
    pub api_base: String,
    pub agent_endpoint_id: String,
    pub agent_endpoint: EndpointRecord,
    pub agent_kem_pubkey: String,
    /// Base64 Ed25519 signing key of the agent. Pinned here so a later
    /// lifecycle command is addressed to the same agent that accepted the
    /// deployment, whatever a scheduler answers next time.
    pub agent_signing_pubkey: String,
    pub service_mesh_expires_at_secs: u64,
    pub update_state: ReplicaUpdateState,
}

/// Everything `podctl` needs to reach every replica of one deployment again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentCatalog {
    pub version: u16,
    pub deployment_id: String,
    pub workload_name: String,
    pub replica_count: u32,
    pub replicas: Vec<ReplicaPlacement>,
}

impl DeploymentCatalog {
    pub fn new(deployment_id: String, workload_name: String, replica_count: u32) -> Self {
        Self {
            version: DEPLOYMENT_CATALOG_VERSION,
            deployment_id,
            workload_name,
            replica_count,
            replicas: Vec::new(),
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == DEPLOYMENT_CATALOG_VERSION,
            "unsupported deployment catalog version"
        );
        ensure!(
            self.replica_count >= 1 && self.replica_count <= protocol::MAX_WORKLOAD_REPLICAS,
            "deployment catalog replica count is invalid"
        );
        ensure!(
            self.replicas.len() <= self.replica_count as usize,
            "deployment catalog contains too many replicas"
        );
        for replica in &self.replicas {
            ensure!(
                replica.replica_index < self.replica_count,
                "deployment catalog replica index is invalid"
            );
            ensure!(
                replica.revision_id == replica.receipt.revision_id,
                "deployment catalog revision does not match receipt"
            );
            replica.receipt.verify()?;
            replica.agent_endpoint.verify_structure()?;
            ensure!(
                hex::encode(&replica.agent_endpoint.endpoint_id) == replica.agent_endpoint_id
                    && replica.agent_endpoint.signing_pubkey == replica.agent_signing_pubkey,
                "deployment catalog agent identity binding is invalid"
            );
        }
        Ok(())
    }
}

/// Directory holding one JSON file per deployment.
pub fn catalog_dir(key_dir: &Path) -> Result<PathBuf> {
    let path = key_dir.join("workloads");
    std::fs::create_dir_all(&path)
        .with_context(|| format!("create catalog directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

fn catalog_path(key_dir: &Path, deployment_id: &str) -> Result<PathBuf> {
    ensure!(
        deployment_id.len() == 64 && deployment_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid deployment id"
    );
    Ok(catalog_dir(key_dir)?.join(format!("{deployment_id}.json")))
}

/// Write the catalog atomically, so a crash mid-write cannot destroy the only
/// record of a running deployment.
pub fn save(key_dir: &Path, catalog: &DeploymentCatalog) -> Result<()> {
    catalog.validate()?;
    let path = catalog_path(key_dir, &catalog.deployment_id)?;
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(catalog)?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("open {}", temporary.display()))?;
    std::io::Write::write_all(&mut file, &bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("commit catalog {}", path.display()))?;
    Ok(())
}

pub fn remove(key_dir: &Path, deployment_id: &str) -> Result<()> {
    std::fs::remove_file(catalog_path(key_dir, deployment_id)?).map_err(Into::into)
}

/// Load a deployment catalog by deployment id or by workload name.
///
/// Names are resolved against the local catalog directory only. `podctl` keeps
/// no cluster-side index, so a name applied from another machine is unknown
/// here and must be addressed by its deployment id.
pub fn load(key_dir: &Path, identifier: &str) -> Result<DeploymentCatalog> {
    if let Ok(path) = catalog_path(key_dir, identifier) {
        let catalog: DeploymentCatalog =
            serde_json::from_slice(&std::fs::read(path).context("deployment catalog not found")?)?;
        catalog.validate()?;
        return Ok(catalog);
    }
    let mut matched: Option<DeploymentCatalog> = None;
    for catalog in load_all(key_dir)? {
        if catalog.workload_name != identifier {
            continue;
        }
        ensure!(
            matched.is_none(),
            "workload name {identifier} is ambiguous, use the deployment id"
        );
        matched = Some(catalog);
    }
    matched.ok_or_else(|| anyhow!("no deployment named {identifier}"))
}

pub fn load_if_exists(key_dir: &Path, deployment_id: &str) -> Result<Option<DeploymentCatalog>> {
    let path = catalog_path(key_dir, deployment_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let catalog: DeploymentCatalog = serde_json::from_slice(&std::fs::read(path)?)?;
    catalog.validate()?;
    Ok(Some(catalog))
}

pub fn load_all(key_dir: &Path) -> Result<Vec<DeploymentCatalog>> {
    let mut catalogs = Vec::new();
    for entry in std::fs::read_dir(catalog_dir(key_dir)?)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        match serde_json::from_slice::<DeploymentCatalog>(&std::fs::read(entry.path())?) {
            Ok(catalog) if catalog.validate().is_ok() => catalogs.push(catalog),
            Ok(_) => log::warn!(
                "skipping invalid deployment catalog {}",
                entry.path().display()
            ),
            Err(error) => log::warn!(
                "skipping unreadable deployment catalog {}: {error}",
                entry.path().display()
            ),
        }
    }
    catalogs.sort_by(|left, right| left.workload_name.cmp(&right.workload_name));
    Ok(catalogs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> DeploymentCatalog {
        DeploymentCatalog::new("a".repeat(64), "demo".into(), 1)
    }

    #[test]
    fn a_saved_catalog_round_trips_by_id_and_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = catalog();
        save(dir.path(), &catalog).unwrap();
        assert_eq!(
            load(dir.path(), &catalog.deployment_id)
                .unwrap()
                .deployment_id,
            catalog.deployment_id
        );
        assert_eq!(load(dir.path(), "demo").unwrap().workload_name, "demo");
    }

    #[test]
    fn saving_twice_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = catalog();
        save(dir.path(), &catalog).unwrap();
        save(dir.path(), &catalog).unwrap();
        let entries: Vec<_> = std::fs::read_dir(catalog_dir(dir.path()).unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec![format!("{}.json", catalog.deployment_id)]);
    }

    #[test]
    fn an_unreadable_catalog_does_not_hide_the_others() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &catalog()).unwrap();
        std::fs::write(catalog_dir(dir.path()).unwrap().join("broken.json"), b"{").unwrap();
        assert_eq!(load_all(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn a_missing_deployment_is_reported_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let error = load(dir.path(), "absent").unwrap_err();
        assert!(error.to_string().contains("no deployment named absent"));
    }
}
