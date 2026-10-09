//! The signed envelope every peer-to-peer message travels in.
//!
//! An envelope binds a payload to the sender, the *intended recipient*, a
//! nonce and a timestamp, and covers all of it with one Ed25519 signature under
//! the [`SignatureDomain::WorkloadEnvelope`] domain. Binding the recipient is
//! what stops a relay from taking an envelope addressed to one peer and
//! presenting it to another; binding the payload type keeps a response from
//! being replayed as a request.
//!
//! There is no unsigned mode. The typed workload-control API is the only
//! supported acceptance path.

use anyhow::{Context, Result, ensure};
use crypto::SignatureDomain;
use serde::{Deserialize, Serialize};

/// Maximum accepted difference between an envelope timestamp and local time.
pub const MAX_ENVELOPE_DRIFT_MS: u64 = 30_000;
/// Largest envelope accepted from the wire.
pub const MAX_ENVELOPE_BYTES: usize = 68 * 1024;
/// The only signature algorithm this protocol admits.
pub const ENVELOPE_SIGNATURE_ALGORITHM: &str = "ed25519";
/// Envelope wire version. Consumers refuse anything else.
pub const ENVELOPE_VERSION: u16 = 1;

/// The envelope as it appears on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Envelope {
    pub version: u16,
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
    pub payload_type: String,
    pub nonce: String,
    pub ts_millis: u64,
    pub alg: String,
    /// Endpoint id of the sender, as authenticated by the transport.
    pub sender_id: String,
    /// Endpoint id this envelope is addressed to. A receiver refuses an
    /// envelope naming anybody else, so a captured envelope cannot be forwarded.
    pub recipient_id: String,
    pub sender_signing_pubkey: String,
    pub sender_kem_pubkey: String,
    pub signature: String,
}

/// Everything needed to produce a signed envelope.
pub struct EnvelopeParts<'a> {
    pub payload: &'a [u8],
    pub payload_type: &'a str,
    pub nonce: &'a str,
    pub ts_millis: u64,
    pub sender_id: &'a str,
    pub recipient_id: &'a str,
    pub sender_signing_pubkey: &'a [u8],
    pub sender_kem_pubkey: Option<&'a [u8]>,
}

impl Envelope {
    /// Bytes covered by the signature: the whole envelope with the signature
    /// field blanked, so every other field is authenticated.
    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let unsigned = Self {
            signature: String::new(),
            ..self.clone()
        };
        postcard::to_allocvec(&unsigned).context("serialize envelope canonical form")
    }

    /// Build and sign an envelope.
    pub fn seal(parts: EnvelopeParts<'_>, signing_private: &[u8]) -> Result<Self> {
        let mut envelope = Self {
            version: ENVELOPE_VERSION,
            payload: parts.payload.to_vec(),
            payload_type: parts.payload_type.to_string(),
            nonce: parts.nonce.to_string(),
            ts_millis: parts.ts_millis,
            alg: ENVELOPE_SIGNATURE_ALGORITHM.to_string(),
            sender_id: parts.sender_id.to_string(),
            recipient_id: parts.recipient_id.to_string(),
            sender_signing_pubkey: crypto::b64_encode(parts.sender_signing_pubkey),
            sender_kem_pubkey: parts
                .sender_kem_pubkey
                .map(crypto::b64_encode)
                .unwrap_or_default(),
            signature: String::new(),
        };
        let canonical = envelope.canonical_bytes()?;
        envelope.signature = crypto::b64_encode(&crypto::sign_domain(
            signing_private,
            SignatureDomain::WorkloadEnvelope,
            &canonical,
        )?);
        Ok(envelope)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.to_bytes_with_limit(MAX_ENVELOPE_BYTES)
    }

    pub(crate) fn to_bytes_with_limit(&self, max_bytes: usize) -> Result<Vec<u8>> {
        let bytes = postcard::to_allocvec(self).context("serialize envelope")?;
        ensure!(
            bytes.len() <= max_bytes,
            "envelope is {} bytes, over the {max_bytes} byte limit",
            bytes.len(),
        );
        Ok(bytes)
    }

    /// Decode without validating. Only the typed workload-control acceptance
    /// path may trust the result.
    pub(crate) fn decode_with_limit(bytes: &[u8], max_bytes: usize) -> Result<Self> {
        ensure!(
            bytes.len() <= max_bytes,
            "envelope is {} bytes, over the {max_bytes} byte limit",
            bytes.len(),
        );
        postcard::from_bytes(bytes).context("decode envelope")
    }

    pub(crate) fn verify_signature(&self) -> Result<()> {
        let signing_key = self.sender_signing_key()?;
        let signature = crypto::b64_decode(&self.signature).context("decode envelope signature")?;
        crypto::verify_domain(
            &signing_key,
            SignatureDomain::WorkloadEnvelope,
            &self.canonical_bytes()?,
            &signature,
        )
        .context("verify envelope signature")
    }

    pub fn sender_signing_key(&self) -> Result<Vec<u8>> {
        crypto::b64_decode(&self.sender_signing_pubkey).context("decode envelope signing key")
    }

    pub fn sender_kem_key(&self) -> Result<Option<Vec<u8>>> {
        if self.sender_kem_pubkey.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            crypto::b64_decode(&self.sender_kem_pubkey).context("decode envelope KEM key")?,
        ))
    }
}
