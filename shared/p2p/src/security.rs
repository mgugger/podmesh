use anyhow::Error;
use libp2p::PeerId;
use thiserror::Error;

use crate::envelope;

// Re-export VerifiedEnvelope from the envelope module for backward compatibility
pub use crate::envelope::VerifiedEnvelope;

/// Signature enforcement failure when signed messages are required.
#[derive(Debug, Error)]
pub enum EnvelopeRejection {
    #[error("signed message required: {0}")]
    SignatureRequired(#[from] Error),
}

/// Determine whether the node must enforce signed messages.
pub fn require_signed_messages() -> bool {
    crypto::envelope_validator::EnvelopeValidator::require_signed_messages()
}

/// Verify a postcard envelope and check nonce for replay protection.
/// Returns the verified envelope on successful verification.
pub fn verify_envelope_and_check_nonce(envelope_bytes: &[u8]) -> anyhow::Result<VerifiedEnvelope> {
    verify_envelope_and_check_nonce_for_peer(envelope_bytes, "global")
}

/// Verify a postcard envelope and check nonce for replay protection for a specific peer.
/// Returns the verified envelope on successful verification.
pub fn verify_envelope_and_check_nonce_for_peer(
    envelope_bytes: &[u8],
    peer_id: &str,
) -> anyhow::Result<VerifiedEnvelope> {
    envelope::verify_envelope_for_peer(envelope_bytes, std::time::Duration::from_secs(300), peer_id)
}

/// Verify the supplied bytes as a signed envelope originating from `peer`.
/// Any verification failure is promoted to an `EnvelopeRejection`.
pub fn verify_signed_payload_for_peer(
    message_bytes: &[u8],
    peer: &PeerId,
) -> Result<VerifiedEnvelope, EnvelopeRejection> {
    verify_envelope_and_check_nonce_for_peer(message_bytes, &peer.to_string())
        .map_err(EnvelopeRejection::SignatureRequired)
}
