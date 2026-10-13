use std::path::PathBuf;

use anyhow::Result;
use iroh_support::NodeIdentity;

/// Where a sidecar's keys come from.
///
/// Each variant produces a distinct identity, so two sidecars in one process
/// never share keys. Current Podman injection uses `Ephemeral`. A runtime can
/// provide workload-owned storage and select `Persistent` (including a guest
/// filesystem); key generation still happens inside the workload boundary.
/// Managed container identity storage and endpoint enrollment are separate work.
#[derive(Clone, Debug)]
pub enum IdentitySource {
    Persistent(PathBuf),
    Ephemeral(std::sync::Arc<NodeIdentity>),
}

impl IdentitySource {
    pub fn ephemeral() -> Self {
        Self::Ephemeral(std::sync::Arc::new(NodeIdentity::ephemeral()))
    }

    pub fn load(&self) -> Result<NodeIdentity> {
        match self {
            Self::Persistent(key_dir) => NodeIdentity::load(key_dir),
            Self::Ephemeral(identity) => Ok(identity.as_ref().clone()),
        }
    }
}
