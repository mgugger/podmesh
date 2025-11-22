use serde::{Deserialize, Serialize};

/// Metadata file written by the machine-plane for gateway sidecars.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySidecarMetadata {
    /// Manifest identifier for the deployed workload.
    pub manifest_id: String,
    /// Original manifest payload encoded as base64 to avoid YAML parsing issues.
    pub manifest_b64: String,
    /// Optional owner public key encoded as base64 (may be empty if unknown).
    pub owner_public_key_b64: Option<String>,
    /// Bootstrap peer multiaddr for the workload DHT.
    pub bootstrap_peer: String,
}

/// Default workload-plane bootstrap peer multiaddr used by gateways.
pub const DEFAULT_GATEWAY_BOOTSTRAP_MULTIADDR: &str = "/dns4/workload-bootstrap/udp/4002/quic-v1";

/// Default mount path inside the workload pod where gateway metadata is exposed.
pub const DEFAULT_METADATA_MOUNT_PATH: &str = "/var/run/podmesh/gateway";
/// File name placed inside the metadata mount.
pub const DEFAULT_METADATA_FILENAME: &str = "metadata.json";
/// Fully-qualified default path for the metadata JSON file.
pub const DEFAULT_METADATA_FILE: &str = "/var/run/podmesh/gateway/metadata.json";
/// Environment variable that conveys the metadata file path to the gateway process.
pub const METADATA_PATH_ENV_VAR: &str = "PODMESH_GATEWAY_METADATA_PATH";
/// Environment variable that optionally overrides the bootstrap peer from metadata.
pub const BOOTSTRAP_ENV_VAR: &str = "PODMESH_GATEWAY_BOOTSTRAP_PEER";
