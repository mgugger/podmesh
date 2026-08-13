use std::path::PathBuf;

use anyhow::Result;
use iroh_support::NodeIdentity;

/// Where a sidecar's keys come from.
///
/// Each variant produces a distinct identity, so two sidecars in one process
/// never share keys. Injected sidecars normally use `Ephemeral`: a pod is
/// disposable and its endpoint identity does not need to outlive it.
#[derive(Clone, Debug)]
pub enum IdentitySource {
    Persistent(PathBuf),
    Ephemeral,
}

impl IdentitySource {
    pub fn ephemeral() -> Self {
        Self::Ephemeral
    }

    pub fn load(&self) -> Result<NodeIdentity> {
        match self {
            Self::Persistent(key_dir) => NodeIdentity::load(key_dir),
            Self::Ephemeral => Ok(NodeIdentity::ephemeral()),
        }
    }
}
