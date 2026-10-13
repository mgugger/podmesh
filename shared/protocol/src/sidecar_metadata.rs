use serde::{Deserialize, Serialize};

use crate::EndpointRecord;

/// Metadata file written by the machine-plane for sidecars.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarMetadata {
    /// Workload name from the manifest. With the owner key it derives the
    /// routing key the proxy indexes this workload under.
    pub workload_name: String,
    /// `route_id(owner_public_key, workload_name)`.
    ///
    /// Shared by every replica of the deployment: they all serve the same
    /// route, and the proxy balances between them.
    pub manifest_id: String,
    /// Which replica of the deployment this pod is.
    pub replica_index: u32,
    /// How many replicas the owner asked for.
    pub replica_count: u32,
    /// Original manifest payload encoded as base64 to avoid YAML parsing issues.
    pub manifest_b64: String,
    /// Base64 Ed25519 key of the namespace owner.
    ///
    /// Required: a sidecar without it cannot verify the Biscuit a proxy
    /// presents, and would therefore serve tenant traffic to any proxy that
    /// happened to be in its metadata.
    pub owner_public_key_b64: String,
    /// Initial tenant proxy identities and dialable addresses.
    pub proxy_endpoints: Vec<EndpointRecord>,
    #[serde(default)]
    pub service_catalog: Option<crate::service_discovery::ServiceCatalog>,
    /// Owner-signed, replica/revision-scoped enrollment delegation naming the
    /// selected agent issuer. This is not itself workload-session authority;
    /// the connector must receive a child bound to its generated endpoint keys.
    pub workload_credential_b64: String,
    /// Workload relay credential delivered only inside the encrypted execution specification.
    pub workload_relay_auth_token: String,
    /// Optional private CA certificates in DER form for workload relay TLS.
    pub workload_relay_ca_certificates: Vec<Vec<u8>>,
}

impl SidecarMetadata {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.workload_name.is_empty() && self.workload_name.len() <= 253,
            "invalid workload name"
        );
        let owner = crypto::b64_decode(&self.owner_public_key_b64)?;
        anyhow::ensure!(
            owner.len() == crypto::ED25519_PUBLIC_KEY_SIZE,
            "owner_public_key_b64 must decode to {} bytes",
            crypto::ED25519_PUBLIC_KEY_SIZE
        );
        anyhow::ensure!(
            self.manifest_id == crate::route_id(&owner, &self.workload_name),
            "manifest_id is not derived from the owner key and workload name"
        );
        anyhow::ensure!(
            self.replica_count >= 1 && self.replica_count <= crate::MAX_WORKLOAD_REPLICAS,
            "replica count must be between 1 and {}",
            crate::MAX_WORKLOAD_REPLICAS
        );
        anyhow::ensure!(
            self.replica_index < self.replica_count,
            "replica index is outside the deployment"
        );
        anyhow::ensure!(
            !self.proxy_endpoints.is_empty()
                && self.proxy_endpoints.len()
                    <= crate::proxy_endpoint_discovery::MAX_PROXY_ENDPOINTS,
            "invalid proxy endpoint count"
        );
        for endpoint in &self.proxy_endpoints {
            // Structure and signature only. A pod may be restarted long after
            // it was deployed, and a stale address can only fail to dial.
            endpoint.verify_structure()?;
        }
        // Signature and binding only. A pod may be restarted, or an agent may
        // reconcile after a restart, long after the credential was minted, and
        // refusing then would delete a healthy long-running workload. Expiry is
        // enforced by the proxy, which is where the authorisation happens.
        let delegation = crate::workload_identity::EnrollmentDelegation::from_b64(
            &self.workload_credential_b64,
        )?;
        delegation.verify_scope(
            &self.owner_public_key_b64,
            &self.manifest_id,
            &crate::workload_id(&owner, &self.workload_name, self.replica_index),
            None,
            None,
        )?;
        anyhow::ensure!(
            delegation.scope.revision_id
                == crate::revision_id(&crypto::b64_decode(&self.manifest_b64)?),
            "sidecar enrollment revision mismatch"
        );
        if let Some(catalog) = &self.service_catalog {
            catalog.verify(&self.owner_public_key_b64)?;
            anyhow::ensure!(
                catalog.manifest_id == self.manifest_id
                    && catalog.revision_id == delegation.scope.revision_id
                    && catalog.services
                        == crate::service_discovery::workload_services(&crypto::b64_decode(
                            &self.manifest_b64
                        )?)?,
                "sidecar service catalog mismatch"
            );
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
