use base64::Engine;
use once_cell::sync::Lazy;
use protocol::gateway_metadata::{
    BOOTSTRAP_ENV_VAR, GatewaySidecarMetadata, METADATA_BLOB_ENV_VAR,
};
use std::{io, sync::Arc};
use tokio::sync::RwLock;

pub use protocol::gateway_metadata::DEFAULT_GATEWAY_BOOTSTRAP_MULTIADDR;

/// Default container image used for gateway sidecars injected into workloads.
pub const DEFAULT_GATEWAY_IMAGE: &str = "podmesh/sidecar";
/// Name assigned to the gateway sidecar container inside injected pods.
pub const GATEWAY_SIDECAR_CONTAINER_NAME: &str = "podmesh-sidecar";
/// Environment variable that provides the inline metadata blob to the gateway.
pub const GATEWAY_METADATA_BLOB_ENV: &str = METADATA_BLOB_ENV_VAR;
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

/// Build an inline metadata blob that can be injected directly into the workload manifest.
pub fn build_inline_metadata_blob(
    manifest_id: &str,
    manifest_bytes: &[u8],
    owner_public_key: &[u8],
    bootstrap_peer: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let metadata = GatewaySidecarMetadata {
        manifest_id: manifest_id.to_string(),
        manifest_b64: base64::engine::general_purpose::STANDARD.encode(manifest_bytes),
        owner_public_key_b64: if owner_public_key.is_empty() {
            None
        } else {
            Some(base64::engine::general_purpose::STANDARD.encode(owner_public_key))
        },
        bootstrap_peer: bootstrap_peer.to_string(),
    };

    let serialized = serde_json::to_vec(&metadata).map_err(|err| {
        io::Error::new(
            io::ErrorKind::Other,
            format!(
                "failed to serialize gateway metadata for manifest {}: {}",
                manifest_id, err
            ),
        )
    })?;

    Ok(base64::engine::general_purpose::STANDARD.encode(serialized))
}
