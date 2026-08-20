use serde::{Deserialize, Serialize};

use crate::EndpointRecord;

pub const AGENT_PROTOCOL_VERSION: u16 = 1;
pub const MAX_ENCRYPTED_CAPSULE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_WRAPPED_KEY_BYTES: usize = 4 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
/// Upper bound on replicas per deployment. Each replica consumes one agent, so
/// this also bounds how many agents a single `podctl apply` can occupy.
pub const MAX_WORKLOAD_REPLICAS: u32 = 64;
/// Longest an owner-signed agent message may stay valid.
///
/// Without an upper bound a client could mint a message that never expires, and
/// a captured `Delete` would stay replayable for the life of the mesh. Five
/// minutes is ample for a control round trip and short enough that the agent's
/// replay cache only has to remember a bounded window.
pub const MAX_AGENT_MESSAGE_LIFETIME_SECS: u64 = 300;
/// Tolerated difference between the signer's clock and the agent's clock.
pub const MAX_AGENT_CLOCK_SKEW_SECS: u64 = 60;
/// Bounds how many workloads one agent reports for an owner in a single answer,
/// matching the largest number of workloads an agent will hold.
pub const MAX_LISTED_WORKLOADS: usize = 10_000;
/// Bounds the unsealed list request, which is broadcast to every agent.
pub const MAX_WORKLOAD_LIST_REQUEST_BYTES: usize = 4 * 1024;

/// Shared freshness rules for every owner-signed agent message.
///
/// Each message names when it was issued and when it expires; the agent checks
/// that the window is plausible, bounded, and currently open.
fn validate_validity_window(
    issued_at_secs: u64,
    expires_at_secs: u64,
    now_secs: u64,
    what: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        issued_at_secs <= now_secs.saturating_add(MAX_AGENT_CLOCK_SKEW_SECS),
        "{what} was issued too far in the future"
    );
    anyhow::ensure!(
        expires_at_secs > issued_at_secs,
        "{what} expires before it was issued"
    );
    anyhow::ensure!(
        expires_at_secs - issued_at_secs <= MAX_AGENT_MESSAGE_LIFETIME_SECS,
        "{what} lifetime exceeds {MAX_AGENT_MESSAGE_LIFETIME_SECS} seconds"
    );
    anyhow::ensure!(
        expires_at_secs.saturating_add(MAX_AGENT_CLOCK_SKEW_SECS) >= now_secs,
        "{what} expired"
    );
    Ok(())
}

fn canonical<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    postcard::to_allocvec(value).map_err(Into::into)
}

fn decode_fixed(value: &str, expected: usize, field: &str) -> anyhow::Result<Vec<u8>> {
    let decoded = crypto::b64_decode(value)?;
    anyhow::ensure!(
        decoded.len() == expected,
        "{field} must decode to {expected} bytes"
    );
    Ok(decoded)
}

fn validate_hex_id(value: &str, field: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() == 64,
        "{field} must be a full 32-byte hex digest"
    );
    anyhow::ensure!(
        value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{field} must be hex"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdmissionRequest {
    pub version: u16,
    pub request_id: String,
    pub namespace_id: String,
    pub workload_id: String,
    /// Base64 Ed25519 signing key of the one agent this request is for.
    ///
    /// Without it a scheduler could fan a single owner-signed request out to
    /// every agent in the mesh and hold capacity everywhere at once, because
    /// each agent's replay cache is local and would see the nonce only once.
    pub target_node_id: String,
    pub response_kem_pubkey: String,
    pub cpu_milli: u32,
    pub memory_bytes: u64,
    pub storage_bytes: u64,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub nonce: String,
    pub owner_signature: String,
}

impl AdmissionRequest {
    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(&Self {
            owner_signature: String::new(),
            ..self.clone()
        })
    }

    pub fn sign(mut self, owner_private: &[u8]) -> anyhow::Result<Self> {
        self.owner_signature.clear();
        self.owner_signature = crypto::b64_encode(&crypto::sign_domain(
            owner_private,
            crypto::SignatureDomain::AdmissionRequest,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == AGENT_PROTOCOL_VERSION,
            "unsupported admission version"
        );
        let owner = decode_fixed(&self.namespace_id, 32, "namespace_id")?;
        decode_fixed(&self.target_node_id, 32, "target_node_id")?;
        decode_fixed(&self.response_kem_pubkey, 32, "response_kem_pubkey")?;
        validate_hex_id(&self.workload_id, "workload_id")?;
        anyhow::ensure!(
            !self.request_id.is_empty() && self.request_id.len() <= 128,
            "invalid request_id"
        );
        anyhow::ensure!(
            !self.nonce.is_empty() && self.nonce.len() <= 128,
            "invalid nonce"
        );
        validate_validity_window(
            self.issued_at_secs,
            self.expires_at_secs,
            now_secs,
            "admission request",
        )?;
        let signature = decode_fixed(&self.owner_signature, 64, "owner_signature")?;
        crypto::verify_domain(
            &owner,
            crypto::SignatureDomain::AdmissionRequest,
            &self.canonical_bytes()?,
            &signature,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reservation {
    pub version: u16,
    pub reservation_id: String,
    pub request_id: String,
    pub namespace_id: String,
    pub workload_id: String,
    pub agent_node_id: String,
    pub cpu_milli: u32,
    pub memory_bytes: u64,
    pub storage_bytes: u64,
    pub accepted: bool,
    pub reason: String,
    pub expires_at_secs: u64,
    pub signature: String,
}

impl Reservation {
    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(&Self {
            signature: String::new(),
            ..self.clone()
        })
    }

    pub fn sign(mut self, signing_public: &[u8], signing_private: &[u8]) -> anyhow::Result<Self> {
        self.agent_node_id = crypto::b64_encode(signing_public);
        self.signature.clear();
        self.signature = crypto::b64_encode(&crypto::sign_domain(
            signing_private,
            crypto::SignatureDomain::Reservation,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == AGENT_PROTOCOL_VERSION,
            "unsupported reservation version"
        );
        let agent = decode_fixed(&self.agent_node_id, 32, "agent_node_id")?;
        decode_fixed(&self.namespace_id, 32, "namespace_id")?;
        validate_hex_id(&self.workload_id, "workload_id")?;
        anyhow::ensure!(self.expires_at_secs >= now_secs, "reservation expired");
        anyhow::ensure!(self.reason.len() <= 1_024, "reservation reason too long");
        let signature = decode_fixed(&self.signature, 64, "signature")?;
        crypto::verify_domain(
            &agent,
            crypto::SignatureDomain::Reservation,
            &self.canonical_bytes()?,
            &signature,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncryptedWorkloadCapsule {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub wrapped_dek: Vec<u8>,
}

impl EncryptedWorkloadCapsule {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.ciphertext.is_empty() && self.ciphertext.len() <= MAX_ENCRYPTED_CAPSULE_BYTES,
            "invalid capsule ciphertext length"
        );
        anyhow::ensure!(self.nonce.len() == 24, "capsule nonce must be 24 bytes");
        anyhow::ensure!(
            !self.wrapped_dek.is_empty() && self.wrapped_dek.len() <= MAX_WRAPPED_KEY_BYTES,
            "invalid wrapped DEK length"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionSpec {
    pub workload_name: String,
    /// Zero-based index of this replica within the deployment.
    pub replica_index: u32,
    /// Total replicas the owner asked for. Every replica is placed on a
    /// distinct agent, and each agent runs exactly one pod for it.
    pub replica_count: u32,
    pub manifest: Vec<u8>,
    pub proxy_endpoints: Vec<crate::EndpointRecord>,
    /// Owner-signed proof that this workload belongs to the deploying tenant.
    pub workload_credential_b64: String,
    pub workload_relay_auth_token: String,
    pub workload_relay_ca_certificates: Vec<Vec<u8>>,
}

impl ExecutionSpec {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.workload_name.is_empty() && self.workload_name.len() <= 253,
            "invalid workload name"
        );
        anyhow::ensure!(
            self.replica_count >= 1 && self.replica_count <= MAX_WORKLOAD_REPLICAS,
            "replica count must be between 1 and {MAX_WORKLOAD_REPLICAS}"
        );
        anyhow::ensure!(
            self.replica_index < self.replica_count,
            "replica index is outside the deployment"
        );
        anyhow::ensure!(
            !self.manifest.is_empty() && self.manifest.len() <= MAX_MANIFEST_BYTES,
            "invalid manifest length"
        );
        anyhow::ensure!(
            !self.proxy_endpoints.is_empty()
                && self.proxy_endpoints.len()
                    <= crate::proxy_endpoint_discovery::MAX_PROXY_ENDPOINTS,
            "invalid proxy endpoint count"
        );
        // Structure and signature only. An execution spec is decrypted again
        // every time the agent reconciles after a restart, and proxy records
        // expire in an hour; demanding freshness here would stop an agent
        // recovering any workload deployed more than an hour ago.
        for endpoint in &self.proxy_endpoints {
            endpoint.verify_structure()?;
        }
        anyhow::ensure!(
            !self.workload_credential_b64.is_empty()
                && self.workload_credential_b64.len() <= crate::MAX_WORKLOAD_CREDENTIAL_B64_LEN,
            "invalid workload credential length"
        );
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentGrant {
    pub version: u16,
    pub namespace_id: String,
    pub workload_id: String,
    pub revision_id: String,
    pub target_node_id: String,
    pub response_kem_pubkey: String,
    pub reservation_id: String,
    pub capsule: EncryptedWorkloadCapsule,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub nonce: String,
    pub owner_signature: String,
}

impl DeploymentGrant {
    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(&Self {
            owner_signature: String::new(),
            ..self.clone()
        })
    }

    pub fn sign(mut self, owner_private: &[u8]) -> anyhow::Result<Self> {
        self.owner_signature.clear();
        self.owner_signature = crypto::b64_encode(&crypto::sign_domain(
            owner_private,
            crypto::SignatureDomain::DeploymentGrant,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == AGENT_PROTOCOL_VERSION,
            "unsupported deployment grant version"
        );
        let owner = decode_fixed(&self.namespace_id, 32, "namespace_id")?;
        decode_fixed(&self.target_node_id, 32, "target_node_id")?;
        decode_fixed(&self.response_kem_pubkey, 32, "response_kem_pubkey")?;
        validate_hex_id(&self.workload_id, "workload_id")?;
        validate_hex_id(&self.revision_id, "revision_id")?;
        self.capsule.validate()?;
        validate_validity_window(
            self.issued_at_secs,
            self.expires_at_secs,
            now_secs,
            "deployment grant",
        )?;
        anyhow::ensure!(
            !self.reservation_id.is_empty() && self.reservation_id.len() <= 128,
            "invalid reservation_id"
        );
        anyhow::ensure!(
            !self.nonce.is_empty() && self.nonce.len() <= 128,
            "invalid nonce"
        );
        let signature = decode_fixed(&self.owner_signature, 64, "owner_signature")?;
        crypto::verify_domain(
            &owner,
            crypto::SignatureDomain::DeploymentGrant,
            &self.canonical_bytes()?,
            &signature,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateRequest {
    pub version: u16,
    pub request_id: String,
    pub namespace_id: String,
    pub workload_id: String,
    pub target_node_id: String,
    pub response_kem_pubkey: String,
    pub expected_revision_id: String,
    pub requested_revision_id: String,
    /// Replace the execution spec even when the application revision is
    /// unchanged, so expiring service-mesh authority can be renewed.
    pub refresh_service_mesh: bool,
    pub cpu_milli: u32,
    pub memory_bytes: u64,
    pub storage_bytes: u64,
    pub grant: DeploymentGrant,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub nonce: String,
    pub owner_signature: String,
}

impl UpdateRequest {
    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(&Self {
            owner_signature: String::new(),
            ..self.clone()
        })
    }

    pub fn sign(mut self, owner_private: &[u8]) -> anyhow::Result<Self> {
        self.owner_signature.clear();
        self.owner_signature = crypto::b64_encode(&crypto::sign_domain(
            owner_private,
            crypto::SignatureDomain::UpdateRequest,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == AGENT_PROTOCOL_VERSION,
            "unsupported update request version"
        );
        let owner = decode_fixed(&self.namespace_id, 32, "namespace_id")?;
        decode_fixed(&self.target_node_id, 32, "target_node_id")?;
        decode_fixed(&self.response_kem_pubkey, 32, "response_kem_pubkey")?;
        validate_hex_id(&self.workload_id, "workload_id")?;
        validate_hex_id(&self.expected_revision_id, "expected_revision_id")?;
        validate_hex_id(&self.requested_revision_id, "requested_revision_id")?;
        anyhow::ensure!(
            !self.request_id.is_empty() && self.request_id.len() <= 128,
            "invalid request_id"
        );
        anyhow::ensure!(
            !self.nonce.is_empty() && self.nonce.len() <= 128,
            "invalid nonce"
        );
        anyhow::ensure!(
            self.cpu_milli > 0 && self.memory_bytes > 0 && self.storage_bytes > 0,
            "update resources must be non-zero"
        );
        self.grant.verify(now_secs)?;
        anyhow::ensure!(
            self.grant.namespace_id == self.namespace_id
                && self.grant.workload_id == self.workload_id
                && self.grant.target_node_id == self.target_node_id
                && self.grant.response_kem_pubkey == self.response_kem_pubkey
                && self.grant.revision_id == self.requested_revision_id,
            "update grant binding mismatch"
        );
        validate_validity_window(
            self.issued_at_secs,
            self.expires_at_secs,
            now_secs,
            "update request",
        )?;
        let signature = decode_fixed(&self.owner_signature, 64, "owner_signature")?;
        crypto::verify_domain(
            &owner,
            crypto::SignatureDomain::UpdateRequest,
            &self.canonical_bytes()?,
            &signature,
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateOutcome {
    Applied,
    AlreadyActive,
    Conflict,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateResponse {
    pub version: u16,
    pub request_id: String,
    pub expected_revision_id: String,
    pub requested_revision_id: String,
    pub receipt: DeploymentReceipt,
    pub outcome: UpdateOutcome,
    pub reason: String,
    pub responded_at_secs: u64,
    pub signature: String,
}

impl UpdateResponse {
    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(&Self {
            signature: String::new(),
            ..self.clone()
        })
    }

    pub fn sign(mut self, signing_public: &[u8], signing_private: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.receipt.agent_node_id == crypto::b64_encode(signing_public),
            "update response receipt signer mismatch"
        );
        self.signature.clear();
        self.signature = crypto::b64_encode(&crypto::sign_domain(
            signing_private,
            crypto::SignatureDomain::UpdateResponse,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == AGENT_PROTOCOL_VERSION,
            "unsupported update response version"
        );
        validate_hex_id(&self.expected_revision_id, "expected_revision_id")?;
        validate_hex_id(&self.requested_revision_id, "requested_revision_id")?;
        self.receipt.verify()?;
        anyhow::ensure!(
            !self.request_id.is_empty() && self.request_id.len() <= 128,
            "invalid request_id"
        );
        anyhow::ensure!(
            self.reason.len() <= 1_024,
            "update response field length is invalid"
        );
        anyhow::ensure!(
            self.responded_at_secs <= now_secs.saturating_add(MAX_AGENT_CLOCK_SKEW_SECS),
            "update response was issued too far in the future"
        );
        let signature = decode_fixed(&self.signature, 64, "signature")?;
        let agent = crypto::b64_decode(&self.receipt.agent_node_id)?;
        crypto::verify_domain(
            &agent,
            crypto::SignatureDomain::UpdateResponse,
            &self.canonical_bytes()?,
            &signature,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentReceipt {
    pub version: u16,
    pub namespace_id: String,
    pub workload_id: String,
    pub revision_id: String,
    pub agent_node_id: String,
    pub runtime_id: String,
    pub accepted_at_secs: u64,
    pub signature: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadOperation {
    Status,
    Logs,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadCommand {
    pub version: u16,
    pub request_id: String,
    pub namespace_id: String,
    pub workload_id: String,
    /// Base64 Ed25519 signing key of the agent this command is addressed to, so
    /// a captured command cannot be re-aimed at another replica's host.
    pub target_node_id: String,
    pub operation: WorkloadOperation,
    pub log_tail: Option<u32>,
    pub response_kem_pubkey: String,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub nonce: String,
    pub owner_signature: String,
}

impl WorkloadCommand {
    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(&Self {
            owner_signature: String::new(),
            ..self.clone()
        })
    }

    pub fn sign(mut self, owner_private: &[u8]) -> anyhow::Result<Self> {
        self.owner_signature.clear();
        self.owner_signature = crypto::b64_encode(&crypto::sign_domain(
            owner_private,
            crypto::SignatureDomain::WorkloadCommand,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == AGENT_PROTOCOL_VERSION,
            "unsupported workload command version"
        );
        let owner = decode_fixed(&self.namespace_id, 32, "namespace_id")?;
        decode_fixed(&self.target_node_id, 32, "target_node_id")?;
        decode_fixed(&self.response_kem_pubkey, 32, "response_kem_pubkey")?;
        validate_hex_id(&self.workload_id, "workload_id")?;
        anyhow::ensure!(
            !self.request_id.is_empty() && self.request_id.len() <= 128,
            "invalid request_id"
        );
        anyhow::ensure!(
            !self.nonce.is_empty() && self.nonce.len() <= 128,
            "invalid nonce"
        );
        validate_validity_window(
            self.issued_at_secs,
            self.expires_at_secs,
            now_secs,
            "workload command",
        )?;
        anyhow::ensure!(
            self.log_tail.unwrap_or(0) <= 10_000,
            "log tail exceeds limit"
        );
        let signature = decode_fixed(&self.owner_signature, 64, "owner_signature")?;
        crypto::verify_domain(
            &owner,
            crypto::SignatureDomain::WorkloadCommand,
            &self.canonical_bytes()?,
            &signature,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadCommandResponse {
    pub version: u16,
    pub request_id: String,
    pub workload_id: String,
    pub agent_node_id: String,
    pub ok: bool,
    pub payload: String,
    pub responded_at_secs: u64,
    pub signature: String,
}

impl WorkloadCommandResponse {
    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(&Self {
            signature: String::new(),
            ..self.clone()
        })
    }

    pub fn sign(mut self, signing_public: &[u8], signing_private: &[u8]) -> anyhow::Result<Self> {
        self.agent_node_id = crypto::b64_encode(signing_public);
        self.signature.clear();
        self.signature = crypto::b64_encode(&crypto::sign_domain(
            signing_private,
            crypto::SignatureDomain::WorkloadCommandResponse,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == AGENT_PROTOCOL_VERSION,
            "unsupported workload response version"
        );
        let agent = decode_fixed(&self.agent_node_id, 32, "agent_node_id")?;
        validate_hex_id(&self.workload_id, "workload_id")?;
        anyhow::ensure!(
            self.payload.len() <= 1024 * 1024,
            "workload response payload too large"
        );
        let signature = decode_fixed(&self.signature, 64, "signature")?;
        crypto::verify_domain(
            &agent,
            crypto::SignatureDomain::WorkloadCommandResponse,
            &self.canonical_bytes()?,
            &signature,
        )
    }
}

impl DeploymentReceipt {
    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(&Self {
            signature: String::new(),
            ..self.clone()
        })
    }

    pub fn sign(mut self, signing_public: &[u8], signing_private: &[u8]) -> anyhow::Result<Self> {
        self.agent_node_id = crypto::b64_encode(signing_public);
        self.signature.clear();
        self.signature = crypto::b64_encode(&crypto::sign_domain(
            signing_private,
            crypto::SignatureDomain::DeploymentReceipt,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self) -> anyhow::Result<()> {
        let agent = decode_fixed(&self.agent_node_id, 32, "agent_node_id")?;
        decode_fixed(&self.namespace_id, 32, "namespace_id")?;
        validate_hex_id(&self.workload_id, "workload_id")?;
        validate_hex_id(&self.revision_id, "revision_id")?;
        anyhow::ensure!(
            self.version == AGENT_PROTOCOL_VERSION,
            "unsupported deployment receipt version"
        );
        let signature = decode_fixed(&self.signature, 64, "signature")?;
        crypto::verify_domain(
            &agent,
            crypto::SignatureDomain::DeploymentReceipt,
            &self.canonical_bytes()?,
            &signature,
        )
    }
}

/// Identifies one deployment within a namespace. It is the stable key a client
/// uses to find every replica of a workload again.
pub fn deployment_id(namespace_id: &[u8], workload_name: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"podmesh/deployment/v1\0");
    hasher.update(namespace_id);
    hasher.update(b"\0");
    hasher.update(workload_name.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Identifies a single replica of a deployment. Each replica is admitted,
/// executed, and deleted independently on its own agent, so every replica needs
/// its own workload identity.
pub fn workload_id(namespace_id: &[u8], workload_name: &str, replica_index: u32) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"podmesh/workload/v1\0");
    hasher.update(namespace_id);
    hasher.update(b"\0");
    hasher.update(workload_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(&replica_index.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}

pub fn revision_id(canonical_manifest: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"podmesh/revision/v1\0");
    hasher.update(canonical_manifest);
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> u64 {
        1_700_000_000
    }

    fn signed_update() -> (UpdateRequest, Vec<u8>, Vec<u8>) {
        let (owner_public, owner_private) = crypto::generate_signing_keypair();
        let (agent_public, _) = crypto::generate_signing_keypair();
        let workload = workload_id(&owner_public, "demo", 0);
        let requested_revision = revision_id(b"requested");
        let grant = DeploymentGrant {
            version: AGENT_PROTOCOL_VERSION,
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: workload.clone(),
            revision_id: requested_revision.clone(),
            target_node_id: crypto::b64_encode(&agent_public),
            response_kem_pubkey: crypto::b64_encode(&[8; 32]),
            reservation_id: "update-request".into(),
            capsule: EncryptedWorkloadCapsule {
                ciphertext: vec![1],
                nonce: vec![2; 24],
                wrapped_dek: vec![3],
            },
            issued_at_secs: now(),
            expires_at_secs: now() + 30,
            nonce: "inner-update".into(),
            owner_signature: String::new(),
        }
        .sign(&owner_private)
        .unwrap();
        let update = UpdateRequest {
            version: AGENT_PROTOCOL_VERSION,
            request_id: "update-request".into(),
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: workload,
            target_node_id: crypto::b64_encode(&agent_public),
            response_kem_pubkey: crypto::b64_encode(&[8; 32]),
            expected_revision_id: revision_id(b"expected"),
            requested_revision_id: requested_revision,
            refresh_service_mesh: false,
            cpu_milli: 100,
            memory_bytes: 1024,
            storage_bytes: 2048,
            grant,
            issued_at_secs: now(),
            expires_at_secs: now() + 30,
            nonce: "outer-update".into(),
            owner_signature: String::new(),
        }
        .sign(&owner_private)
        .unwrap();
        (update, owner_public, agent_public)
    }

    #[test]
    fn deployment_grant_rejects_tampered_capsule() {
        let (owner_public, owner_private) = crypto::generate_signing_keypair();
        let (agent_public, _) = crypto::generate_signing_keypair();
        let response_kem = [9u8; 32];
        let mut grant = DeploymentGrant {
            version: AGENT_PROTOCOL_VERSION,
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: workload_id(&owner_public, "demo", 0),
            revision_id: revision_id(b"manifest"),
            target_node_id: crypto::b64_encode(&agent_public),
            response_kem_pubkey: crypto::b64_encode(&response_kem),
            reservation_id: "reservation".into(),
            capsule: EncryptedWorkloadCapsule {
                ciphertext: vec![1],
                nonce: vec![0; 24],
                wrapped_dek: vec![3],
            },
            issued_at_secs: now(),
            expires_at_secs: now() + 30,
            nonce: "n-2".into(),
            owner_signature: String::new(),
        }
        .sign(&owner_private)
        .unwrap();
        grant.verify(now()).unwrap();
        grant.capsule.ciphertext[0] ^= 1;
        assert!(grant.verify(now()).is_err());
    }

    #[test]
    fn workload_identity_is_namespaced_and_revision_is_content_addressed() {
        assert_ne!(
            workload_id(&[1; 32], "demo", 0),
            workload_id(&[2; 32], "demo", 0)
        );
        assert_ne!(revision_id(b"a"), revision_id(b"b"));
        assert_eq!(revision_id(b"a").len(), 64);
    }

    #[test]
    fn every_replica_gets_its_own_workload_identity() {
        let first = workload_id(&[1; 32], "demo", 0);
        let second = workload_id(&[1; 32], "demo", 1);
        assert_ne!(first, second);
        assert_eq!(first, workload_id(&[1; 32], "demo", 0));
        assert_ne!(first, deployment_id(&[1; 32], "demo"));
    }

    #[test]
    fn deployment_identity_is_stable_across_replica_counts() {
        assert_eq!(
            deployment_id(&[1; 32], "demo"),
            deployment_id(&[1; 32], "demo")
        );
        assert_ne!(
            deployment_id(&[1; 32], "demo"),
            deployment_id(&[2; 32], "demo")
        );
    }

    #[test]
    fn update_request_binds_target_revisions_and_freshness() {
        let (update, _, _) = signed_update();
        update.verify(now()).unwrap();
        for mutate in [
            |value: &mut UpdateRequest| value.target_node_id = crypto::b64_encode(&[9; 32]),
            |value: &mut UpdateRequest| value.expected_revision_id = revision_id(b"stale"),
            |value: &mut UpdateRequest| value.requested_revision_id = revision_id(b"substitute"),
            |value: &mut UpdateRequest| value.response_kem_pubkey = crypto::b64_encode(&[7; 32]),
        ] {
            let mut tampered = update.clone();
            mutate(&mut tampered);
            assert!(tampered.verify(now()).is_err());
        }
        assert!(
            update
                .verify(now() + MAX_AGENT_MESSAGE_LIFETIME_SECS + 100)
                .is_err()
        );
    }

    #[test]
    fn update_request_rejects_oversized_capsules_and_cross_operation_signatures() {
        let (mut update, owner_public, _) = signed_update();
        update.grant.capsule.ciphertext = vec![0; MAX_ENCRYPTED_CAPSULE_BYTES + 1];
        assert!(update.verify(now()).is_err());

        let signature = crypto::b64_decode(&update.owner_signature).unwrap();
        assert!(
            crypto::verify_domain(
                &owner_public,
                crypto::SignatureDomain::DeploymentGrant,
                &update.canonical_bytes().unwrap(),
                &signature,
            )
            .is_err()
        );
    }

    #[test]
    fn update_response_signature_prevents_receipt_substitution() {
        let (update, _, agent_public) = signed_update();
        let (_, agent_private) = crypto::generate_signing_keypair();
        let receipt = DeploymentReceipt {
            version: AGENT_PROTOCOL_VERSION,
            namespace_id: update.namespace_id.clone(),
            workload_id: update.workload_id.clone(),
            revision_id: update.requested_revision_id.clone(),
            agent_node_id: String::new(),
            runtime_id: "pod-demo".into(),
            accepted_at_secs: now(),
            signature: String::new(),
        }
        .sign(&agent_public, &agent_private)
        .unwrap();
        assert!(
            receipt.verify().is_err(),
            "mismatched keypair must be rejected"
        );

        let (agent_public, agent_private) = crypto::generate_signing_keypair();
        let receipt = DeploymentReceipt {
            version: AGENT_PROTOCOL_VERSION,
            namespace_id: update.namespace_id.clone(),
            workload_id: update.workload_id.clone(),
            revision_id: update.requested_revision_id.clone(),
            agent_node_id: String::new(),
            runtime_id: "pod-demo".into(),
            accepted_at_secs: now(),
            signature: String::new(),
        }
        .sign(&agent_public, &agent_private)
        .unwrap();
        let mut response = UpdateResponse {
            version: AGENT_PROTOCOL_VERSION,
            request_id: update.request_id,
            expected_revision_id: update.expected_revision_id,
            requested_revision_id: update.requested_revision_id,
            receipt,
            outcome: UpdateOutcome::Applied,
            reason: String::new(),
            responded_at_secs: now(),
            signature: String::new(),
        }
        .sign(&agent_public, &agent_private)
        .unwrap();
        response.verify(now()).unwrap();
        response.receipt.runtime_id = "substituted".into();
        assert!(response.verify(now()).is_err());
    }
}

/// Asks one agent which of an owner's workloads it holds.
///
/// `podctl` places replicas itself and keeps the only index of where they went,
/// so a lost or stale catalog otherwise leaves workloads running with no way to
/// find them. This is the query that makes them discoverable again.
///
/// Unlike every other agent message this one is neither sealed nor bound to a
/// single agent, because it carries nothing secret and changes nothing: it is
/// the owner's public key, a response key, and a signature. That is what lets a
/// scheduler broadcast one request verbatim to every agent, which is necessary
/// because a client that has lost its catalog does not know which agents to ask.
///
/// An agent answers only with workloads belonging to the signing key, and seals
/// the answer to `response_kem_pubkey`. Replaying a captured request therefore
/// yields answers only the owner can read.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadListRequest {
    pub version: u16,
    pub request_id: String,
    pub namespace_id: String,
    pub response_kem_pubkey: String,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub nonce: String,
    pub owner_signature: String,
}

impl WorkloadListRequest {
    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(&Self {
            owner_signature: String::new(),
            ..self.clone()
        })
    }

    pub fn sign(mut self, owner_private: &[u8]) -> anyhow::Result<Self> {
        self.owner_signature = crypto::b64_encode(&crypto::sign_domain(
            owner_private,
            crypto::SignatureDomain::WorkloadListRequest,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == AGENT_PROTOCOL_VERSION,
            "unsupported workload list version"
        );
        let owner = decode_fixed(&self.namespace_id, 32, "namespace_id")?;
        decode_fixed(&self.response_kem_pubkey, 32, "response_kem_pubkey")?;
        anyhow::ensure!(
            !self.request_id.is_empty() && self.request_id.len() <= 128,
            "invalid request_id"
        );
        anyhow::ensure!(
            !self.nonce.is_empty() && self.nonce.len() <= 128,
            "invalid nonce"
        );
        validate_validity_window(
            self.issued_at_secs,
            self.expires_at_secs,
            now_secs,
            "workload list request",
        )?;
        let signature = decode_fixed(&self.owner_signature, 64, "owner_signature")?;
        crypto::verify_domain(
            &owner,
            crypto::SignatureDomain::WorkloadListRequest,
            &self.canonical_bytes()?,
            &signature,
        )
    }

    /// Wire form. The request travels unsealed, so it is encoded directly
    /// rather than wrapped in a recipient-bound blob.
    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(self)
    }

    pub fn from_bytes(bytes: &[u8], now_secs: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            bytes.len() <= MAX_WORKLOAD_LIST_REQUEST_BYTES,
            "workload list request exceeds {MAX_WORKLOAD_LIST_REQUEST_BYTES} bytes"
        );
        let request: Self = postcard::from_bytes(bytes)?;
        request.verify(now_secs)?;
        Ok(request)
    }
}

/// One workload an agent holds for the requesting owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadSummary {
    pub workload_id: String,
    pub workload_name: String,
    pub revision_id: String,
    pub replica_index: u32,
    pub replica_count: u32,
    /// Runtime state as the agent's runtime reports it.
    pub state: String,
    pub deploying_since_secs: u64,
    /// Fresh agent-signed proof of the current accepted revision.
    pub receipt: DeploymentReceipt,
}

/// An agent's answer to [`WorkloadListRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadListResponse {
    pub version: u16,
    pub request_id: String,
    pub agent_node_id: String,
    pub agent_endpoint: EndpointRecord,
    pub agent_kem_pubkey: String,
    pub workloads: Vec<WorkloadSummary>,
    pub responded_at_secs: u64,
    pub signature: String,
}

impl WorkloadListResponse {
    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(&Self {
            signature: String::new(),
            ..self.clone()
        })
    }

    pub fn sign(mut self, signing_public: &[u8], signing_private: &[u8]) -> anyhow::Result<Self> {
        self.agent_node_id = crypto::b64_encode(signing_public);
        self.signature = crypto::b64_encode(&crypto::sign_domain(
            signing_private,
            crypto::SignatureDomain::WorkloadListResponse,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == AGENT_PROTOCOL_VERSION,
            "unsupported workload list response version"
        );
        let agent = decode_fixed(&self.agent_node_id, 32, "agent_node_id")?;
        decode_fixed(&self.agent_kem_pubkey, 32, "agent_kem_pubkey")?;
        self.agent_endpoint.verify(self.responded_at_secs)?;
        anyhow::ensure!(
            self.agent_endpoint.signing_pubkey == self.agent_node_id,
            "workload list endpoint does not belong to the responding agent"
        );
        anyhow::ensure!(
            self.workloads.len() <= MAX_LISTED_WORKLOADS,
            "workload list response exceeds {MAX_LISTED_WORKLOADS} entries"
        );
        for workload in &self.workloads {
            validate_hex_id(&workload.workload_id, "workload_id")?;
            validate_hex_id(&workload.revision_id, "revision_id")?;
            anyhow::ensure!(
                workload.state.len() <= 128 && workload.workload_name.len() <= 253,
                "workload summary field exceeds its bound"
            );
            workload.receipt.verify()?;
            anyhow::ensure!(
                workload.receipt.workload_id == workload.workload_id
                    && workload.receipt.revision_id == workload.revision_id
                    && workload.receipt.agent_node_id == self.agent_node_id,
                "workload summary receipt does not match the reported placement"
            );
        }
        let signature = decode_fixed(&self.signature, 64, "signature")?;
        crypto::verify_domain(
            &agent,
            crypto::SignatureDomain::WorkloadListResponse,
            &self.canonical_bytes()?,
            &signature,
        )
    }
}
