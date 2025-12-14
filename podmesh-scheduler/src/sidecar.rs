use once_cell::sync::Lazy;
use protocol::sidecar_metadata::{
    BOOTSTRAP_ENV_VAR, SidecarMetadata, METADATA_BLOB_ENV_VAR,
};
use std::{io, sync::Arc};
use tokio::sync::RwLock;

pub use protocol::sidecar_metadata::DEFAULT_SIDECAR_BOOTSTRAP_MULTIADDR;

/// Default container image used for sidecars injected into workloads.
pub const DEFAULT_SIDECAR_IMAGE: &str = "podmesh/sidecar";
/// Name assigned to the sidecar container inside injected pods.
pub const SIDECAR_CONTAINER_NAME: &str = "podmesh-sidecar";
/// Environment variable that provides the inline metadata blob to the sidecar.
pub const SIDECAR_METADATA_BLOB_ENV: &str = METADATA_BLOB_ENV_VAR;
/// Environment variable that provides a direct bootstrap peer override to the sidecar.
pub const SIDECAR_BOOTSTRAP_ENV: &str = BOOTSTRAP_ENV_VAR;
/// Environment variable configuring the sidecar's log verbosity.
pub const SIDECAR_LOG_ENV: &str = "RUST_LOG";
/// Default log level used for injected sidecars.
pub const SIDECAR_LOG_LEVEL: &str = "debug";
/// Environment variable to enable transparent egress proxy in the sidecar.
pub const SIDECAR_ENABLE_EGRESS_ENV: &str = "PODMESH_ENABLE_EGRESS";

/// Settings that control global sidecar behavior.
#[derive(Debug, Clone)]
pub struct SidecarSettings {
    /// Container image reference (registry/name:tag).
    pub image: String,
    /// Bootstrap peer multiaddr that the sidecar should dial.
    pub bootstrap_peer: String,
}

impl Default for SidecarSettings {
    fn default() -> Self {
        Self {
            image: DEFAULT_SIDECAR_IMAGE.to_string(),
            bootstrap_peer: DEFAULT_SIDECAR_BOOTSTRAP_MULTIADDR.to_string(),
        }
    }
}

static SETTINGS: Lazy<Arc<RwLock<SidecarSettings>>> =
    Lazy::new(|| Arc::new(RwLock::new(SidecarSettings::default())));

/// Update the global sidecar settings from CLI/configuration sources.
pub async fn set_sidecar_settings(settings: SidecarSettings) {
    let mut guard = SETTINGS.write().await;
    *guard = settings;
}

/// Fetch the currently configured sidecar settings.
pub async fn sidecar_settings() -> SidecarSettings {
    SETTINGS.read().await.clone()
}

/// Build an inline metadata blob that can be injected directly into the workload manifest.
pub fn build_inline_metadata_blob(
    manifest_id: &str,
    manifest_bytes: &[u8],
    owner_public_key: &[u8],
    bootstrap_peer: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let metadata = SidecarMetadata {
        manifest_id: manifest_id.to_string(),
        manifest_b64: crypto::b64_encode(manifest_bytes),
        owner_public_key_b64: if owner_public_key.is_empty() {
            None
        } else {
            Some(crypto::b64_encode(owner_public_key))
        },
        bootstrap_peer: bootstrap_peer.to_string(),
    };

    let serialized = serde_json::to_vec(&metadata).map_err(|err| {
        io::Error::new(
            io::ErrorKind::Other,
            format!(
                "failed to serialize sidecar metadata for manifest {}: {}",
                manifest_id, err
            ),
        )
    })?;

    Ok(crypto::b64_encode(&serialized))
}
