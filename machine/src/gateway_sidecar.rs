use once_cell::sync::Lazy;
use protocol::gateway_metadata::{
    BOOTSTRAP_ENV_VAR, DEFAULT_METADATA_FILE, DEFAULT_METADATA_FILENAME,
    DEFAULT_METADATA_MOUNT_PATH, METADATA_PATH_ENV_VAR,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

pub use protocol::gateway_metadata::DEFAULT_GATEWAY_BOOTSTRAP_MULTIADDR;

/// Default container image used for gateway sidecars injected into workloads.
pub const DEFAULT_GATEWAY_IMAGE: &str = "podmesh/gateway";
/// Host-side directory prefix where gateway metadata files are written.
pub const GATEWAY_METADATA_HOST_ROOT: &str = "/var/lib/podmesh/sidecar";
/// Container path where the metadata volume is mounted.
pub const GATEWAY_METADATA_MOUNT_PATH: &str = DEFAULT_METADATA_MOUNT_PATH;
/// File name placed inside the metadata mount for the gateway runtime to read.
pub const GATEWAY_METADATA_FILENAME: &str = DEFAULT_METADATA_FILENAME;
/// Name assigned to the gateway sidecar container inside injected pods.
pub const GATEWAY_SIDECAR_CONTAINER_NAME: &str = "podmesh-sidecar";
/// Volume name used for the gateway metadata mount.
pub const GATEWAY_VOLUME_NAME: &str = "podmesh-sidecar-metadata";
/// Environment variable that provides the metadata file path to the gateway.
pub const GATEWAY_METADATA_ENV: &str = METADATA_PATH_ENV_VAR;
/// Environment variable that provides a direct bootstrap peer override to the gateway.
pub const GATEWAY_BOOTSTRAP_ENV: &str = BOOTSTRAP_ENV_VAR;
/// Environment variable configuring the gateway's log verbosity.
pub const GATEWAY_LOG_ENV: &str = "RUST_LOG";
/// Default log level used for injected gateway sidecars.
pub const GATEWAY_LOG_LEVEL: &str = "info";

/// Settings that control global gateway sidecar behavior.
#[derive(Debug, Clone)]
pub struct GatewaySidecarSettings {
    /// Container image reference (registry/name:tag).
    pub image: String,
    /// Bootstrap peer multiaddr that the gateway should dial.
    pub bootstrap_peer: String,
}

impl Default for GatewaySidecarSettings {
    fn default() -> Self {
        Self {
            image: DEFAULT_GATEWAY_IMAGE.to_string(),
            bootstrap_peer: DEFAULT_GATEWAY_BOOTSTRAP_MULTIADDR.to_string(),
        }
    }
}

static SETTINGS: Lazy<Arc<RwLock<GatewaySidecarSettings>>> =
    Lazy::new(|| Arc::new(RwLock::new(GatewaySidecarSettings::default())));

/// Update the global gateway sidecar settings from CLI/configuration sources.
pub async fn set_gateway_sidecar_settings(settings: GatewaySidecarSettings) {
    let mut guard = SETTINGS.write().await;
    *guard = settings;
}

/// Fetch the currently configured gateway sidecar settings.
pub async fn gateway_sidecar_settings() -> GatewaySidecarSettings {
    SETTINGS.read().await.clone()
}

/// Compute the host directory used to store gateway metadata for a manifest.
pub fn metadata_host_dir(manifest_id: &str) -> PathBuf {
    PathBuf::from(GATEWAY_METADATA_HOST_ROOT).join(sanitize_manifest_id(manifest_id))
}

/// Compute the host file path for the gateway metadata JSON.
pub fn metadata_file_path(manifest_id: &str) -> PathBuf {
    metadata_host_dir(manifest_id).join(GATEWAY_METADATA_FILENAME)
}

/// Compute the container path for the gateway metadata JSON file.
pub fn metadata_container_path() -> String {
    DEFAULT_METADATA_FILE.to_string()
}

fn sanitize_manifest_id(manifest_id: &str) -> String {
    manifest_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}
