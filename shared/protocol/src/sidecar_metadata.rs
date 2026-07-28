use serde::{Deserialize, Serialize};

use crate::EndpointRecord;

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
    pub proxy_endpoints: Vec<EndpointRecord>,
    /// Workload relay credential delivered only inside the encrypted execution specification.
    pub workload_relay_auth_token: String,
    /// Optional private CA certificates in DER form for workload relay TLS.
    pub workload_relay_ca_certificates: Vec<Vec<u8>>,
}

impl SidecarMetadata {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.proxy_endpoints.is_empty()
                && self.proxy_endpoints.len()
                    <= crate::proxy_endpoint_discovery::MAX_PROXY_ENDPOINTS,
            "invalid proxy endpoint count"
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        for endpoint in &self.proxy_endpoints {
            endpoint.verify(now)?;
        }
        anyhow::ensure!(
            self.workload_relay_auth_token.len() >= 32
                && self.workload_relay_auth_token.len() <= 4 * 1024,
            "invalid workload relay auth token length"
        );
        anyhow::ensure!(
            self.workload_relay_ca_certificates.len() <= 8,
            "too many workload relay CA certificates"
        );
        for certificate in &self.workload_relay_ca_certificates {
            anyhow::ensure!(
                !certificate.is_empty() && certificate.len() <= 64 * 1024,
                "invalid workload relay CA certificate size"
            );
        }
        Ok(())
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
