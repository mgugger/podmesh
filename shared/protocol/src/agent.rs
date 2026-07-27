use serde::{Deserialize, Serialize};

pub const AGENT_PROTOCOL_VERSION: u16 = 1;
pub const MAX_AGENT_ADDRESS_LEN: usize = 2_048;
pub const MAX_CAPABILITIES: usize = 64;
pub const MAX_CAPABILITY_LEN: usize = 128;
pub const MAX_ENCRYPTED_CAPSULE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_WRAPPED_KEY_BYTES: usize = 4 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

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
pub struct AgentAdvertisement {
    pub version: u16,
    pub node_id: String,
    pub kem_pubkey: String,
    pub relay_url: String,
    pub capabilities: Vec<String>,
    pub available: bool,
    pub load_percent: u8,
    pub expires_at_secs: u64,
    pub nonce: String,
    pub signature: String,
}

impl AgentAdvertisement {
    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        canonical(&Self {
            signature: String::new(),
            ..self.clone()
        })
    }

    pub fn validate(&self, now_secs: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == AGENT_PROTOCOL_VERSION,
            "unsupported agent protocol version"
        );
        decode_fixed(&self.node_id, 32, "node_id")?;
        decode_fixed(&self.kem_pubkey, 32, "kem_pubkey")?;
        anyhow::ensure!(
            !self.relay_url.is_empty() && self.relay_url.len() <= MAX_AGENT_ADDRESS_LEN,
            "invalid relay_url length"
        );
        anyhow::ensure!(
            self.relay_url.starts_with("http://") || self.relay_url.starts_with("https://"),
            "relay_url must use http or https"
        );
        anyhow::ensure!(
            self.capabilities.len() <= MAX_CAPABILITIES,
            "too many capabilities"
        );
        anyhow::ensure!(
            self.capabilities
                .iter()
                .all(|capability| !capability.is_empty() && capability.len() <= MAX_CAPABILITY_LEN),
            "invalid capability length"
        );
        anyhow::ensure!(self.load_percent <= 100, "load_percent must be <= 100");
        anyhow::ensure!(
            self.expires_at_secs >= now_secs,
            "agent advertisement expired"
        );
        anyhow::ensure!(
            !self.nonce.is_empty() && self.nonce.len() <= 128,
            "invalid nonce"
        );
        Ok(())
    }

    pub fn sign(mut self, signing_public: &[u8], signing_private: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            signing_public.len() == 32,
            "signing public key must be 32 bytes"
        );
        self.node_id = crypto::b64_encode(signing_public);
        self.signature.clear();
        let signature = crypto::sign_data_with_key(signing_private, &self.canonical_bytes()?)?;
        self.signature = crypto::b64_encode(&signature);
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> anyhow::Result<()> {
        self.validate(now_secs)?;
        let public = decode_fixed(&self.node_id, 32, "node_id")?;
        let signature = decode_fixed(&self.signature, 64, "signature")?;
        crypto::verify_envelope(&public, &self.canonical_bytes()?, &signature)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdmissionRequest {
    pub version: u16,
    pub request_id: String,
    pub namespace_id: String,
    pub workload_id: String,
    pub response_kem_pubkey: String,
    pub cpu_milli: u32,
    pub memory_bytes: u64,
    pub storage_bytes: u64,
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
        self.owner_signature = crypto::b64_encode(&crypto::sign_data_with_key(
            owner_private,
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
        anyhow::ensure!(
            self.expires_at_secs >= now_secs,
            "admission request expired"
        );
        let signature = decode_fixed(&self.owner_signature, 64, "owner_signature")?;
        crypto::verify_envelope(&owner, &self.canonical_bytes()?, &signature)
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
        self.signature = crypto::b64_encode(&crypto::sign_data_with_key(
            signing_private,
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
        crypto::verify_envelope(&agent, &self.canonical_bytes()?, &signature)
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
    pub manifest: Vec<u8>,
    pub proxy_peers: Vec<crate::ProxyPeer>,
}

impl ExecutionSpec {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.workload_name.is_empty() && self.workload_name.len() <= 253,
            "invalid workload name"
        );
        anyhow::ensure!(
            !self.manifest.is_empty() && self.manifest.len() <= MAX_MANIFEST_BYTES,
            "invalid manifest length"
        );
        crate::validate_proxy_peers(&self.proxy_peers, false)?;
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
        self.owner_signature = crypto::b64_encode(&crypto::sign_data_with_key(
            owner_private,
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
        anyhow::ensure!(
            self.issued_at_secs <= now_secs,
            "deployment grant issued in the future"
        );
        anyhow::ensure!(self.expires_at_secs >= now_secs, "deployment grant expired");
        anyhow::ensure!(
            !self.reservation_id.is_empty() && self.reservation_id.len() <= 128,
            "invalid reservation_id"
        );
        anyhow::ensure!(
            !self.nonce.is_empty() && self.nonce.len() <= 128,
            "invalid nonce"
        );
        let signature = decode_fixed(&self.owner_signature, 64, "owner_signature")?;
        crypto::verify_envelope(&owner, &self.canonical_bytes()?, &signature)
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
    pub operation: WorkloadOperation,
    pub log_tail: Option<u32>,
    pub response_kem_pubkey: String,
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
        self.owner_signature = crypto::b64_encode(&crypto::sign_data_with_key(
            owner_private,
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
        anyhow::ensure!(self.expires_at_secs >= now_secs, "workload command expired");
        anyhow::ensure!(
            self.log_tail.unwrap_or(0) <= 10_000,
            "log tail exceeds limit"
        );
        let signature = decode_fixed(&self.owner_signature, 64, "owner_signature")?;
        crypto::verify_envelope(&owner, &self.canonical_bytes()?, &signature)
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
        self.signature = crypto::b64_encode(&crypto::sign_data_with_key(
            signing_private,
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
        crypto::verify_envelope(&agent, &self.canonical_bytes()?, &signature)
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
        self.signature = crypto::b64_encode(&crypto::sign_data_with_key(
            signing_private,
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
        crypto::verify_envelope(&agent, &self.canonical_bytes()?, &signature)
    }
}

pub fn workload_id(namespace_id: &[u8], workload_name: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"podmesh/workload/v1\0");
    hasher.update(namespace_id);
    hasher.update(b"\0");
    hasher.update(workload_name.as_bytes());
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
    fn advertisement_signature_covers_address_and_capacity() {
        let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
        let kem_public = [7u8; 32];
        let advertisement = AgentAdvertisement {
            version: AGENT_PROTOCOL_VERSION,
            node_id: String::new(),
            kem_pubkey: crypto::b64_encode(&kem_public),
            relay_url: "http://127.0.0.1:3100".into(),
            capabilities: vec!["podman".into()],
            available: true,
            load_percent: 4,
            expires_at_secs: now() + 30,
            nonce: "n-1".into(),
            signature: String::new(),
        }
        .sign(&public, &private)
        .unwrap();

        advertisement.verify(now()).unwrap();
        let mut tampered = advertisement;
        tampered.load_percent = 3;
        assert!(tampered.verify(now()).is_err());
    }

    #[test]
    fn deployment_grant_rejects_tampered_capsule() {
        let (owner_public, owner_private) = crypto::ensure_keypair_ephemeral().unwrap();
        let (agent_public, _) = crypto::ensure_keypair_ephemeral().unwrap();
        let response_kem = [9u8; 32];
        let mut grant = DeploymentGrant {
            version: AGENT_PROTOCOL_VERSION,
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: workload_id(&owner_public, "demo"),
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
        assert_ne!(workload_id(&[1; 32], "demo"), workload_id(&[2; 32], "demo"));
        assert_ne!(revision_id(b"a"), revision_id(b"b"));
        assert_eq!(revision_id(b"a").len(), 64);
    }
}
