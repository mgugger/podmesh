//! The signed envelope every peer-to-peer message travels in.
//!
//! An envelope binds a payload to the sender, the *intended recipient*, a
//! nonce and a timestamp, and covers all of it with one Ed25519 signature under
//! the [`SignatureDomain::WorkloadHandshake`] domain. Binding the recipient is
//! what stops a relay from taking an envelope addressed to one peer and
//! presenting it to another; binding the payload type keeps a response from
//! being replayed as a request.
//!
//! There is no unsigned mode. [`EnvelopeValidator`] is the only supported way to
//! accept an envelope, and it refuses anything that is unsigned, misaddressed,
//! replayed, or outside the clock-drift window.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use crypto::SignatureDomain;
use serde::{Deserialize, Serialize};

/// Maximum accepted difference between an envelope timestamp and local time.
pub const MAX_ENVELOPE_DRIFT_MS: u64 = 90_000;
/// How long a nonce is remembered, so a replay cannot outlive the drift window.
pub const ENVELOPE_NONCE_WINDOW: Duration = Duration::from_secs(300);
/// Largest envelope accepted from the wire.
pub const MAX_ENVELOPE_BYTES: usize = 64 * 1024;
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
    fn canonical_bytes(&self) -> Result<Vec<u8>> {
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
            SignatureDomain::WorkloadHandshake,
            &canonical,
        )?);
        Ok(envelope)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let bytes = postcard::to_allocvec(self).context("serialize envelope")?;
        ensure!(
            bytes.len() <= MAX_ENVELOPE_BYTES,
            "envelope is {} bytes, over the {MAX_ENVELOPE_BYTES} byte limit",
            bytes.len()
        );
        Ok(bytes)
    }

    /// Decode without validating. Callers must run [`EnvelopeValidator::accept`]
    /// before trusting any field.
    fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_ENVELOPE_BYTES,
            "envelope is {} bytes, over the {MAX_ENVELOPE_BYTES} byte limit",
            bytes.len()
        );
        postcard::from_bytes(bytes).context("decode envelope")
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

/// The single entry point for accepting an envelope.
///
/// Every check is mandatory. There is deliberately no permissive mode: an
/// envelope that cannot be attributed to a specific sender, addressed to this
/// receiver, and shown to be fresh is refused.
pub struct EnvelopeValidator {
    /// Endpoint id of the receiver, taken from the authenticated transport.
    local_id: String,
    /// Endpoint id of the peer, taken from the authenticated transport.
    remote_id: String,
    now_millis: u64,
}

impl EnvelopeValidator {
    pub fn new(local_id: &str, remote_id: &str, now_millis: u64) -> Self {
        Self {
            local_id: local_id.to_string(),
            remote_id: remote_id.to_string(),
            now_millis,
        }
    }

    /// Decode and fully validate an envelope of the expected payload type.
    pub fn accept(&self, bytes: &[u8], expected_payload_type: &str) -> Result<Envelope> {
        let envelope = Envelope::decode(bytes)?;

        ensure!(
            envelope.version == ENVELOPE_VERSION,
            "unsupported envelope version {}",
            envelope.version
        );
        ensure!(
            envelope.alg == ENVELOPE_SIGNATURE_ALGORITHM,
            "unsupported envelope signature algorithm {:?}",
            envelope.alg
        );
        ensure!(
            envelope.payload_type == expected_payload_type,
            "expected envelope payload type {expected_payload_type:?}, got {:?}",
            envelope.payload_type
        );
        // The sender field must agree with the transport identity, so an
        // envelope cannot claim to come from a peer other than the one that
        // completed the QUIC handshake.
        ensure!(
            envelope.sender_id == self.remote_id,
            "envelope sender does not match the authenticated transport"
        );
        // The recipient field must be us. Without this a relay could forward a
        // valid envelope to a third party and impersonate the sender there.
        ensure!(
            envelope.recipient_id == self.local_id,
            "envelope is addressed to another endpoint"
        );
        ensure!(
            self.now_millis.abs_diff(envelope.ts_millis) <= MAX_ENVELOPE_DRIFT_MS,
            "envelope timestamp is outside the accepted drift window"
        );
        ensure!(!envelope.nonce.is_empty(), "envelope nonce is missing");
        ensure!(
            !envelope.signature.is_empty(),
            "envelope is unsigned and unsigned envelopes are never accepted"
        );

        let signing_key = envelope.sender_signing_key()?;
        let signature =
            crypto::b64_decode(&envelope.signature).context("decode envelope signature")?;
        crypto::verify_domain(
            &signing_key,
            SignatureDomain::WorkloadHandshake,
            &envelope.canonical_bytes()?,
            &signature,
        )
        .context("verify envelope signature")?;

        // Record the nonce only after the signature verifies, so an attacker
        // cannot burn nonces on behalf of an honest peer.
        crypto::nonce_helper::check_and_insert_nonce_for_peer(
            &envelope.nonce,
            ENVELOPE_NONCE_WINDOW,
            &self.remote_id,
        )
        .context("envelope replay check")?;

        Ok(envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts<'a>(
        signing_public: &'a [u8],
        sender: &'a str,
        recipient: &'a str,
        nonce: &'a str,
        now: u64,
    ) -> EnvelopeParts<'a> {
        EnvelopeParts {
            payload: b"payload",
            payload_type: "handshake-request",
            nonce,
            ts_millis: now,
            sender_id: sender,
            recipient_id: recipient,
            sender_signing_pubkey: signing_public,
            sender_kem_pubkey: None,
        }
    }

    #[test]
    fn round_trip_accepts_a_well_formed_envelope() {
        let (public, private) = crypto::generate_signing_keypair();
        let now = 1_700_000_000_000;
        let envelope =
            Envelope::seal(parts(&public, "alice", "bob", "nonce-1", now), &private).unwrap();
        let bytes = envelope.to_bytes().unwrap();
        EnvelopeValidator::new("bob", "alice", now)
            .accept(&bytes, "handshake-request")
            .expect("envelope should verify");
    }

    #[test]
    fn envelope_addressed_elsewhere_is_refused() {
        let (public, private) = crypto::generate_signing_keypair();
        let now = 1_700_000_000_000;
        let envelope =
            Envelope::seal(parts(&public, "alice", "bob", "nonce-2", now), &private).unwrap();
        let bytes = envelope.to_bytes().unwrap();
        let error = EnvelopeValidator::new("carol", "alice", now)
            .accept(&bytes, "handshake-request")
            .unwrap_err();
        assert!(error.to_string().contains("addressed to another endpoint"));
    }

    #[test]
    fn envelope_from_another_sender_is_refused() {
        let (public, private) = crypto::generate_signing_keypair();
        let now = 1_700_000_000_000;
        let envelope =
            Envelope::seal(parts(&public, "alice", "bob", "nonce-3", now), &private).unwrap();
        let bytes = envelope.to_bytes().unwrap();
        let error = EnvelopeValidator::new("bob", "mallory", now)
            .accept(&bytes, "handshake-request")
            .unwrap_err();
        assert!(error.to_string().contains("authenticated transport"));
    }

    #[test]
    fn response_cannot_be_replayed_as_a_request() {
        let (public, private) = crypto::generate_signing_keypair();
        let now = 1_700_000_000_000;
        let mut request = parts(&public, "alice", "bob", "nonce-4", now);
        request.payload_type = "handshake-response";
        let bytes = Envelope::seal(request, &private)
            .unwrap()
            .to_bytes()
            .unwrap();
        let error = EnvelopeValidator::new("bob", "alice", now)
            .accept(&bytes, "handshake-request")
            .unwrap_err();
        assert!(error.to_string().contains("payload type"));
    }

    #[test]
    fn unsigned_envelope_is_refused() {
        let (public, private) = crypto::generate_signing_keypair();
        let now = 1_700_000_000_000;
        let mut envelope =
            Envelope::seal(parts(&public, "alice", "bob", "nonce-5", now), &private).unwrap();
        envelope.signature = String::new();
        let bytes = envelope.to_bytes().unwrap();
        let error = EnvelopeValidator::new("bob", "alice", now)
            .accept(&bytes, "handshake-request")
            .unwrap_err();
        assert!(error.to_string().contains("unsigned"));
    }

    #[test]
    fn tampered_payload_is_refused() {
        let (public, private) = crypto::generate_signing_keypair();
        let now = 1_700_000_000_000;
        let mut envelope =
            Envelope::seal(parts(&public, "alice", "bob", "nonce-6", now), &private).unwrap();
        envelope.payload = b"other payload".to_vec();
        let bytes = envelope.to_bytes().unwrap();
        assert!(
            EnvelopeValidator::new("bob", "alice", now)
                .accept(&bytes, "handshake-request")
                .is_err()
        );
    }

    #[test]
    fn timestamp_outside_the_drift_window_is_refused() {
        let (public, private) = crypto::generate_signing_keypair();
        let now = 1_700_000_000_000;
        let envelope = Envelope::seal(
            parts(
                &public,
                "alice",
                "bob",
                "nonce-7",
                now - MAX_ENVELOPE_DRIFT_MS - 1,
            ),
            &private,
        )
        .unwrap();
        let bytes = envelope.to_bytes().unwrap();
        let error = EnvelopeValidator::new("bob", "alice", now)
            .accept(&bytes, "handshake-request")
            .unwrap_err();
        assert!(error.to_string().contains("drift window"));
    }

    #[test]
    fn replayed_nonce_is_refused() {
        let (public, private) = crypto::generate_signing_keypair();
        let now = 1_700_000_000_000;
        let bytes = Envelope::seal(
            parts(&public, "replay-sender", "replay-receiver", "nonce-8", now),
            &private,
        )
        .unwrap()
        .to_bytes()
        .unwrap();
        let validator = EnvelopeValidator::new("replay-receiver", "replay-sender", now);
        validator.accept(&bytes, "handshake-request").unwrap();
        let error = EnvelopeValidator::new("replay-receiver", "replay-sender", now)
            .accept(&bytes, "handshake-request")
            .unwrap_err();
        assert!(error.to_string().contains("replay"));
    }
}
