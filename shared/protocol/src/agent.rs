use serde::{Deserialize, Serialize};

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
}

/// An agent's answer to [`WorkloadListRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadListResponse {
    pub version: u16,
    pub request_id: String,
    pub agent_node_id: String,
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
