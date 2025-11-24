use anyhow::Result;
use p2p::envelope::{SignEnvelopeConfig, sign_with_existing_keypair};
use protocol::machine::root_as_envelope;

/// Helper for building signed envelopes instead of JSON
pub struct SignedEnvelopeBuilder {
    peer_id: String,
}

impl SignedEnvelopeBuilder {
    #[allow(dead_code)]
    pub fn with_keys(peer_id: String, _public_key: String) -> Self {
        Self { peer_id }
    }

    /// Build a signed manifest envelope containing encrypted manifest payload
    #[allow(dead_code)]
    pub fn build_manifest_envelope(
        &mut self,
        ciphertext: &[u8],
        _nonce_bytes: &[u8],
        _n: usize,
        _k: usize,
        _count: usize,
        sk_bytes: &[u8],
        pk_bytes: &[u8],
    ) -> Result<Vec<u8>> {
        let config = SignEnvelopeConfig {
            peer_id: Some(&self.peer_id),
            ..Default::default()
        };

        Ok(sign_with_existing_keypair(ciphertext, "manifest", config, pk_bytes, sk_bytes)?.bytes)
    }

    /// Add signature and pubkey to an existing envelope
    #[allow(dead_code)]
    pub fn sign_envelope(
        &self,
        envelope_bytes: &[u8],
        sk_bytes: &[u8],
        pk_bytes: &[u8],
    ) -> Result<Vec<u8>> {
        let envelope = root_as_envelope(envelope_bytes)
            .map_err(|e| anyhow::anyhow!("Failed to parse envelope: {}", e))?;

        let payload_bytes = envelope.payload().unwrap_or(&[]);
        let payload_type = envelope.payload_type().unwrap_or("");
        let nonce = envelope.nonce().unwrap_or("");
        let timestamp = envelope.ts();
        let alg = envelope.alg().unwrap_or("ml-dsa-65");
        let kem = envelope.kem_pubkey();

        let config = SignEnvelopeConfig {
            nonce: Some(nonce),
            timestamp: Some(timestamp),
            peer_id: Some(&self.peer_id),
            kem_pub_b64: kem,
            algorithm: alg,
            signature_prefix: alg,
        };

        Ok(
            sign_with_existing_keypair(payload_bytes, payload_type, config, pk_bytes, sk_bytes)?
                .bytes,
        )
    }
}
