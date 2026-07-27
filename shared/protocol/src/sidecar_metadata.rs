use serde::{Deserialize, Serialize};

use crate::{ProxyPeer, validate_proxy_peers};

/// Metadata file written by the machine-plane for sidecars.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarMetadata {
    /// Manifest identifier for the deployed workload.
    pub manifest_id: String,
    /// Original manifest payload encoded as base64 to avoid YAML parsing issues.
    pub manifest_b64: String,
    /// Optional owner public key encoded as base64 (may be empty if unknown).
    pub owner_public_key_b64: Option<String>,
    /// Initial tenant proxy identities and dialable addresses.
    pub proxy_peers: Vec<ProxyPeer>,
}

impl SidecarMetadata {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_proxy_peers(&self.proxy_peers, false)
    }
}

/// Default mount path inside the workload pod where sidecar metadata is exposed.
pub const DEFAULT_METADATA_MOUNT_PATH: &str = "/var/run/podmesh/sidecar";
/// File name placed inside the metadata mount.
pub const DEFAULT_METADATA_FILENAME: &str = "metadata.json";
/// Fully-qualified default path for the metadata JSON file.
pub const DEFAULT_METADATA_FILE: &str = "/var/run/podmesh/sidecar/metadata.json";
/// Environment variable that conveys the metadata file path to the sidecar process.
pub const METADATA_PATH_ENV_VAR: &str = "PODMESH_SIDECAR_METADATA_PATH";
/// Environment variable that conveys an inline base64-encoded metadata blob to the sidecar.
pub const METADATA_BLOB_ENV_VAR: &str = "PODMESH_SIDECAR_METADATA_B64";
