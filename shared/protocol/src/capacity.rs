use std::collections::HashSet;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{EndpointRecord, IROH_ENDPOINT_ID_BYTES};

pub const CAPACITY_PROTOCOL_VERSION: u16 = 1;
pub const MAX_CAPACITY_MESSAGE_BYTES: usize = 16 * 1024;
pub const MAX_CAPACITY_QUERY_LIFETIME_SECS: u64 = 15;
pub const MAX_CAPACITY_OFFER_LIFETIME_SECS: u64 = 15;
pub const MAX_CAPACITY_CLOCK_SKEW_SECS: u64 = 5;
pub const MAX_CAPACITY_CAPABILITIES: usize = 32;
pub const MAX_CAPACITY_CAPABILITY_LEN: usize = 64;
pub const MAX_CAPACITY_EXCLUDED_ENDPOINTS: usize = 64;
pub const MAX_CAPACITY_ID_LEN: usize = 128;
pub const MAX_CAPACITY_NONCE_LEN: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapacityQuery {
    pub version: u16,
    pub query_id: String,
    pub nonce: String,
    pub cpu_milli: u32,
    pub memory_bytes: u64,
    pub storage_bytes: u64,
    pub required_capabilities: Vec<String>,
    pub excluded_endpoint_ids: Vec<Vec<u8>>,
    pub reply_endpoint: EndpointRecord,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub signing_pubkey: String,
    pub signature: String,
}

impl CapacityQuery {
    pub fn sign(
        mut self,
        signing_public: &[u8],
        signing_private: &[u8],
        now_secs: u64,
    ) -> Result<Self> {
        self.signing_pubkey = crypto::b64_encode(signing_public);
        self.signature.clear();
        self.validate_unsigned(now_secs)?;
        self.signature = crypto::b64_encode(&crypto::sign_data_with_key(
            signing_private,
            &self.canonical_bytes()?,
        )?);
        self.validate(now_secs)?;
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> Result<()> {
        self.validate(now_secs)?;
        verify_signature(
            &self.signing_pubkey,
            &self.signature,
            &self.canonical_bytes()?,
        )
    }

    pub fn to_bytes(&self, now_secs: u64) -> Result<Vec<u8>> {
        self.verify(now_secs)?;
        encode_bounded(self, "capacity query")
    }

    pub fn from_bytes(bytes: &[u8], now_secs: u64) -> Result<Self> {
        validate_message_size(bytes)?;
        let value: Self = postcard::from_bytes(bytes).context("decode capacity query")?;
        value.verify(now_secs)?;
        Ok(value)
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        encode_bounded(
            &Self {
                signature: String::new(),
                ..self.clone()
            },
            "canonical capacity query",
        )
    }

    fn validate(&self, now_secs: u64) -> Result<()> {
        self.validate_unsigned(now_secs)?;
        validate_signature_fields(&self.signing_pubkey, &self.signature)
    }

    fn validate_unsigned(&self, now_secs: u64) -> Result<()> {
        validate_common(
            self.version,
            &self.query_id,
            self.issued_at_secs,
            self.expires_at_secs,
            MAX_CAPACITY_QUERY_LIFETIME_SECS,
            now_secs,
        )?;
        ensure!(
            !self.nonce.is_empty() && self.nonce.len() <= MAX_CAPACITY_NONCE_LEN,
            "capacity query nonce length is invalid"
        );
        ensure!(
            self.cpu_milli > 0 && self.memory_bytes > 0 && self.storage_bytes > 0,
            "capacity query resources must be non-zero"
        );
        validate_capabilities(&self.required_capabilities)?;
        validate_exclusions(&self.excluded_endpoint_ids)?;
        self.reply_endpoint.verify(now_secs)?;
        ensure!(
            self.reply_endpoint.signing_pubkey == self.signing_pubkey,
            "capacity query signer is not bound to reply endpoint"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapacityOffer {
    pub version: u16,
    pub query_id: String,
    pub agent_endpoint: EndpointRecord,
    pub kem_pubkey: String,
    pub available_cpu_milli: u32,
    pub available_memory_bytes: u64,
    pub available_storage_bytes: u64,
    pub capabilities: Vec<String>,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub signing_pubkey: String,
    pub signature: String,
}

impl CapacityOffer {
    pub fn sign(
        mut self,
        signing_public: &[u8],
        signing_private: &[u8],
        now_secs: u64,
    ) -> Result<Self> {
        self.signing_pubkey = crypto::b64_encode(signing_public);
        self.signature.clear();
        self.validate_unsigned(now_secs)?;
        self.signature = crypto::b64_encode(&crypto::sign_data_with_key(
            signing_private,
            &self.canonical_bytes()?,
        )?);
        self.validate(now_secs)?;
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> Result<()> {
        self.validate(now_secs)?;
        verify_signature(
            &self.signing_pubkey,
            &self.signature,
            &self.canonical_bytes()?,
        )
    }

    pub fn to_bytes(&self, now_secs: u64) -> Result<Vec<u8>> {
        self.verify(now_secs)?;
        encode_bounded(self, "capacity offer")
    }

    pub fn from_bytes(bytes: &[u8], now_secs: u64) -> Result<Self> {
        validate_message_size(bytes)?;
        let value: Self = postcard::from_bytes(bytes).context("decode capacity offer")?;
        value.verify(now_secs)?;
        Ok(value)
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        encode_bounded(
            &Self {
                signature: String::new(),
                ..self.clone()
            },
            "canonical capacity offer",
        )
    }

    fn validate(&self, now_secs: u64) -> Result<()> {
        self.validate_unsigned(now_secs)?;
        validate_signature_fields(&self.signing_pubkey, &self.signature)
    }

    fn validate_unsigned(&self, now_secs: u64) -> Result<()> {
        validate_common(
            self.version,
            &self.query_id,
            self.issued_at_secs,
            self.expires_at_secs,
            MAX_CAPACITY_OFFER_LIFETIME_SECS,
            now_secs,
        )?;
        self.agent_endpoint.verify(now_secs)?;
        ensure!(
            self.agent_endpoint.signing_pubkey == self.signing_pubkey,
            "capacity offer signer is not bound to agent endpoint"
        );
        let kem_public = crypto::b64_decode(&self.kem_pubkey)?;
        ensure!(
            kem_public.len() == 32,
            "KEM public key must decode to 32 bytes"
        );
        validate_capabilities(&self.capabilities)
    }
}

fn validate_common(
    version: u16,
    id: &str,
    issued_at_secs: u64,
    expires_at_secs: u64,
    max_lifetime_secs: u64,
    now_secs: u64,
) -> Result<()> {
    ensure!(
        version == CAPACITY_PROTOCOL_VERSION,
        "unsupported capacity protocol version"
    );
    ensure!(
        !id.is_empty() && id.len() <= MAX_CAPACITY_ID_LEN,
        "capacity ID length is invalid"
    );
    ensure!(
        issued_at_secs <= now_secs.saturating_add(MAX_CAPACITY_CLOCK_SKEW_SECS),
        "capacity message issue time is too far in the future"
    );
    ensure!(expires_at_secs >= now_secs, "capacity message expired");
    ensure!(
        expires_at_secs >= issued_at_secs,
        "capacity expiry precedes issue time"
    );
    ensure!(
        expires_at_secs.saturating_sub(issued_at_secs) <= max_lifetime_secs,
        "capacity message lifetime exceeds limit"
    );
    Ok(())
}

fn validate_capabilities(capabilities: &[String]) -> Result<()> {
    ensure!(
        capabilities.len() <= MAX_CAPACITY_CAPABILITIES,
        "too many capacity capabilities"
    );
    let mut unique = HashSet::with_capacity(capabilities.len());
    for capability in capabilities {
        ensure!(
            !capability.is_empty() && capability.len() <= MAX_CAPACITY_CAPABILITY_LEN,
            "capacity capability length is invalid"
        );
        ensure!(unique.insert(capability), "duplicate capacity capability");
    }
    Ok(())
}

fn validate_exclusions(endpoint_ids: &[Vec<u8>]) -> Result<()> {
    ensure!(
        endpoint_ids.len() <= MAX_CAPACITY_EXCLUDED_ENDPOINTS,
        "too many excluded endpoints"
    );
    let mut unique = HashSet::with_capacity(endpoint_ids.len());
    for endpoint_id in endpoint_ids {
        ensure!(
            endpoint_id.len() == IROH_ENDPOINT_ID_BYTES,
            "excluded endpoint ID must contain 32 bytes"
        );
        ensure!(unique.insert(endpoint_id), "duplicate excluded endpoint ID");
    }
    Ok(())
}

fn validate_signature_fields(signing_pubkey: &str, signature: &str) -> Result<()> {
    ensure!(
        crypto::b64_decode(signing_pubkey)?.len() == 32,
        "signing key must decode to 32 bytes"
    );
    ensure!(
        crypto::b64_decode(signature)?.len() == 64,
        "signature must decode to 64 bytes"
    );
    Ok(())
}

fn verify_signature(signing_pubkey: &str, signature: &str, canonical: &[u8]) -> Result<()> {
    let public = crypto::b64_decode(signing_pubkey)?;
    let signature = crypto::b64_decode(signature)?;
    crypto::verify_envelope(&public, canonical, &signature)
}

fn encode_bounded(value: &impl Serialize, field: &str) -> Result<Vec<u8>> {
    let bytes = postcard::to_allocvec(value).with_context(|| format!("serialize {field}"))?;
    validate_message_size(&bytes)?;
    Ok(bytes)
}

fn validate_message_size(bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_CAPACITY_MESSAGE_BYTES,
        "capacity message encoded size is invalid"
    );
    Ok(())
}
