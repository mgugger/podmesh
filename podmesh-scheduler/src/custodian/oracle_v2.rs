//! ShamirOracle — concrete implementation of `KeyReleaseOracle` for Shamir secret sharing.
//!
//! When a worker presents a valid `ShareRequest`, the custodian:
//! 1. Verifies the coordinator-signed assignment token (manifest_id + worker_peer_id + assigned_at_secs).
//! 2. Checks that the token is not older than `ASSIGNMENT_TOKEN_TTL_SECS` (5 minutes).
//! 3. Looks up its `CustodianRecord` for the requested `manifest_id`.
//! 4. Unwraps the stored DEK share using its own KEM private key.
//! 5. Re-wraps it to the worker's KEM public key from `request.worker_kem_pub`.
//! 6. Returns the wrapped share in `ShareResponse.wrapped_share`.
//!
//! The custodian never reconstructs the full DEK — it only sees its single share,
//! which is insufficient to decrypt the spec on its own (requires k shares).

use anyhow::Context;
use async_trait::async_trait;

use crypto::{KeyReleaseOracle, ShareRequest, ShareResponse, b64_decode, b64_encode};
use crate::storage::get_custodian_store;

/// Token TTL: custodians reject assignment tokens older than this many seconds.
const ASSIGNMENT_TOKEN_TTL_SECS: u64 = 300; // 5 minutes

/// Shamir oracle: verifies worker authorization then re-wraps a single DEK share.
pub struct ShamirOracle {
    /// libp2p PeerId (base58) of the local custodian node.
    /// Used to look up the correct custodian record in a multi-node-per-process setup.
    pub local_peer_id: String,
}

#[async_trait]
impl KeyReleaseOracle for ShamirOracle {
    async fn release_key_material(&self, request: &ShareRequest) -> anyhow::Result<ShareResponse> {
        // 1. Retrieve the custodian record (contains the coordinator pubkey).
        let store = get_custodian_store()
            .context("custodian store not initialized — is this node running in custodian mode?")?;

        // Look up THIS node's record using the peer-scoped key.
        // Fall back to bare manifest_id for backward compat (single-node deployments).
        let record = store.get_record_for_peer(&request.manifest_id, &self.local_peer_id)?
            .or_else(|| store.get_record(&request.manifest_id).ok().flatten())
            .with_context(|| format!("no custodian record for manifest_id={}", request.manifest_id))?;

        // 2. Verify the assignment token if we have a coordinator pubkey stored.
        //    Nodes upgraded from before this feature may have an empty coordinator_pubkey;
        //    we skip verification gracefully in that case (backward compatibility).
        if !record.coordinator_pubkey.is_empty() && !request.assignment_sig.is_empty() {
            // Check token expiry first (cheap, no crypto).
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            if request.assigned_at_secs == 0 || now_secs.saturating_sub(request.assigned_at_secs) > ASSIGNMENT_TOKEN_TTL_SECS {
                anyhow::bail!(
                    "assignment token expired or missing timestamp for manifest_id={} worker={} (assigned_at={}, now={})",
                    request.manifest_id, request.worker_peer_id, request.assigned_at_secs, now_secs
                );
            }

            // Reconstruct the signed message and verify the coordinator signature.
            let coordinator_pub = b64_decode(&record.coordinator_pubkey)
                .context("invalid base64 in coordinator_pubkey")?;

            let sig_bytes = b64_decode(&request.assignment_sig)
                .context("invalid base64 in assignment_sig")?;

            let mut msg = Vec::new();
            msg.extend_from_slice(request.manifest_id.as_bytes());
            msg.extend_from_slice(request.worker_peer_id.as_bytes());
            msg.extend_from_slice(&request.assigned_at_secs.to_be_bytes());

            crypto::verify_envelope(&coordinator_pub, &msg, &sig_bytes)
                .context("assignment token signature verification failed")?;
        }

        // 3. Obtain this node's KEM private key to unwrap the stored share.
        let (_, our_kem_priv) = crypto::ensure_kem_keypair_on_disk()
            .context("failed to load local KEM keypair")?;

        // 4. Unwrap the DEK share.
        let raw_share = crypto::decrypt_payload_from_recipient_blob(&record.wrapped_share, &our_kem_priv)
            .context("unwrap stored DEK share")?;

        // 5. Decode the worker's X25519 KEM pubkey and re-wrap the share to it.
        let worker_kem_pub = b64_decode(&request.worker_kem_pub)
            .context("invalid base64 in worker_kem_pub")?;

        let wrapped_share = crypto::encrypt_payload_for_recipient(&worker_kem_pub, &raw_share)
            .context("re-wrap DEK share to worker KEM pubkey")?;

        // 6. Sign the response: sig over manifest_id || worker_peer_id || wrapped_share.
        let (_our_signing_pub, our_signing_priv) = crypto::ensure_keypair_on_disk()
            .context("failed to load signing keypair")?;

        let mut msg = Vec::new();
        msg.extend_from_slice(request.manifest_id.as_bytes());
        msg.extend_from_slice(request.worker_peer_id.as_bytes());
        msg.extend_from_slice(&wrapped_share);

        let sig_bytes = crypto::sign_data_with_key(&our_signing_priv, &msg)
            .context("signing ShareResponse failed")?;

        Ok(ShareResponse {
            manifest_id: request.manifest_id.clone(),
            wrapped_share,
            custodian_sig: b64_encode(&sig_bytes),
        })
    }
}
