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
use protocol::{
    AuthzContext, AuthzDecision, AuthzOperation, BiscuitReleaseShareVerifier, NodeCert,
    biscuit_public_key_from_ed25519_bytes, verify_authz_token,
};
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
        // 0. Verify authz token with Biscuit and release_share fact bindings.
        let worker_cert = NodeCert::from_bytes(&request.node_cert_bytes)
            .context("invalid worker node_cert_bytes in ShareRequest")?;
        let worker_signing_pub = b64_decode(&worker_cert.signing_pubkey)
            .context("invalid base64 worker signing_pubkey in node cert")?;
        let root_public_key = biscuit_public_key_from_ed25519_bytes(&worker_signing_pub)
            .context("invalid worker public key for biscuit verification")?;

        let now_unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // 1. Retrieve the custodian record (contains the coordinator pubkey).
        let store = get_custodian_store()
            .context("custodian store not initialized — is this node running in custodian mode?")?;

        // Look up THIS node's record using the peer-scoped key.
        // Fall back to bare manifest_id for backward compat (single-node deployments).
        let record = store.get_record_for_peer(&request.manifest_id, &self.local_peer_id)?
            .or_else(|| store.get_record(&request.manifest_id).ok().flatten())
            .with_context(|| format!("no custodian record for manifest_id={}", request.manifest_id))?;

        let expected_share_index = u32::from(record.share_index);
        if let Some(requested_idx) = request.share_index {
            if requested_idx != expected_share_index {
                anyhow::bail!(
                    "share index mismatch for manifest_id={} requested={} expected={}",
                    request.manifest_id,
                    requested_idx,
                    expected_share_index
                );
            }
        }

        let authz_ctx = AuthzContext {
            tenant_owner_pubkey_b64: worker_cert.owner_pubkey.clone(),
            manifest_id: request.manifest_id.clone(),
            transport_peer_id: request.worker_peer_id.clone(),
            operation: AuthzOperation::ReleaseShare,
            http_path: None,
            dest_host: None,
            dest_port: None,
            worker_peer_id: Some(request.worker_peer_id.clone()),
            share_index: Some(expected_share_index),
            delegate_peer_id: None,
            now_unix_secs,
        };

        let verifier = BiscuitReleaseShareVerifier { root_public_key };
        if let AuthzDecision::Deny { reason } =
            verify_authz_token(request.authz_token_b64.as_deref(), &authz_ctx, &verifier)
        {
            anyhow::bail!("share release denied by authz policy: {}", reason);
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{CustodianRecord, get_custodian_store, init_custodian_store};
    use protocol::{NodeCert, NodeRole, mint_release_share_token_b64};

    fn build_worker_cert(worker_signing_pub: &[u8], owner_pub: &[u8], owner_sk: &[u8]) -> NodeCert {
        NodeCert {
            peer_id: "worker-peer".to_string(),
            kem_pubkey: crypto::b64_encode(&[7u8; 32]),
            signing_pubkey: crypto::b64_encode(worker_signing_pub),
            capabilities: vec!["worker".to_string()],
            role: NodeRole::Worker,
            valid_until: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 3600,
            owner_pubkey: crypto::b64_encode(owner_pub),
            owner_sig: String::new(),
            endorsements: vec![],
        }
        .sign(owner_sk, owner_pub)
        .unwrap()
    }

    fn seed_record(manifest_id: &str, local_peer_id: &str, share_index: u8) {
        init_custodian_store(true).unwrap();
        let store = get_custodian_store().expect("custodian store initialized");

        let (kem_pub, _kem_priv) = crypto::ensure_kem_keypair_on_disk().unwrap();
        let wrapped_share = crypto::encrypt_payload_for_recipient(&kem_pub, b"demo-share").unwrap();

        let record = CustodianRecord::new(
            manifest_id.to_string(),
            "owner".to_string(),
            3,
            2,
            share_index,
            wrapped_share,
            vec![local_peer_id.to_string()],
        )
        .with_local_peer_id(local_peer_id.to_string());
        store.set_record(&record).unwrap();
    }

    #[tokio::test]
    async fn release_share_rejects_when_request_share_index_mismatch() {
        let manifest_id = "m-neg-share-idx-mismatch";
        let local_peer_id = "local-custodian-a";
        seed_record(manifest_id, local_peer_id, 1);

        let (worker_pub, worker_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let (owner_pub, owner_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let worker_cert = build_worker_cert(&worker_pub, &owner_pub, &owner_sk);

        let authz_ctx = AuthzContext {
            tenant_owner_pubkey_b64: worker_cert.owner_pubkey.clone(),
            manifest_id: manifest_id.to_string(),
            transport_peer_id: "worker-1".to_string(),
            operation: AuthzOperation::ReleaseShare,
            http_path: None,
            dest_host: None,
            dest_port: None,
            worker_peer_id: Some("worker-1".to_string()),
            share_index: Some(1),
            delegate_peer_id: None,
            now_unix_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        let token = mint_release_share_token_b64(&worker_sk, &authz_ctx).unwrap();

        let (worker_kem_pub, _worker_kem_priv) = crypto::ensure_kem_keypair_on_disk().unwrap();
        let req = ShareRequest {
            manifest_id: manifest_id.to_string(),
            worker_peer_id: "worker-1".to_string(),
            node_cert_bytes: worker_cert.to_bytes(),
            assignment_sig: String::new(),
            assigned_at_secs: 0,
            share_index: Some(2), // wrong index, record expects 1
            authz_token_b64: Some(token),
            worker_kem_pub: crypto::b64_encode(&worker_kem_pub),
            nonce: "n".to_string(),
            sig: String::new(),
        };

        let oracle = ShamirOracle {
            local_peer_id: local_peer_id.to_string(),
        };
        let err = oracle.release_key_material(&req).await.unwrap_err();
        assert!(
            err.to_string().contains("share index mismatch"),
            "unexpected error: {}",
            err
        );
    }

    #[tokio::test]
    async fn release_share_rejects_when_token_share_index_differs() {
        let manifest_id = "m-neg-token-idx-mismatch";
        let local_peer_id = "local-custodian-b";
        seed_record(manifest_id, local_peer_id, 1);

        let (worker_pub, worker_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let (owner_pub, owner_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let worker_cert = build_worker_cert(&worker_pub, &owner_pub, &owner_sk);

        let authz_ctx_wrong = AuthzContext {
            tenant_owner_pubkey_b64: worker_cert.owner_pubkey.clone(),
            manifest_id: manifest_id.to_string(),
            transport_peer_id: "worker-2".to_string(),
            operation: AuthzOperation::ReleaseShare,
            http_path: None,
            dest_host: None,
            dest_port: None,
            worker_peer_id: Some("worker-2".to_string()),
            share_index: Some(2), // token bound to wrong index
            delegate_peer_id: None,
            now_unix_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        let token = mint_release_share_token_b64(&worker_sk, &authz_ctx_wrong).unwrap();

        let (worker_kem_pub, _worker_kem_priv) = crypto::ensure_kem_keypair_on_disk().unwrap();
        let req = ShareRequest {
            manifest_id: manifest_id.to_string(),
            worker_peer_id: "worker-2".to_string(),
            node_cert_bytes: worker_cert.to_bytes(),
            assignment_sig: String::new(),
            assigned_at_secs: 0,
            share_index: Some(1),
            authz_token_b64: Some(token),
            worker_kem_pub: crypto::b64_encode(&worker_kem_pub),
            nonce: "n".to_string(),
            sig: String::new(),
        };

        let oracle = ShamirOracle {
            local_peer_id: local_peer_id.to_string(),
        };
        let err = oracle.release_key_material(&req).await.unwrap_err();
        assert!(
            err.to_string().contains("authz verification failed")
                || err.to_string().contains("share release denied by authz policy"),
            "unexpected error: {}",
            err
        );
    }

    #[tokio::test]
    async fn release_share_allows_when_share_index_and_token_match() {
        let manifest_id = "m-pos-share-idx-match";
        let local_peer_id = "local-custodian-c";
        seed_record(manifest_id, local_peer_id, 1);

        let (worker_pub, worker_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let (owner_pub, owner_sk) = crypto::ensure_keypair_ephemeral().unwrap();
        let worker_cert = build_worker_cert(&worker_pub, &owner_pub, &owner_sk);

        let authz_ctx = AuthzContext {
            tenant_owner_pubkey_b64: worker_cert.owner_pubkey.clone(),
            manifest_id: manifest_id.to_string(),
            transport_peer_id: "worker-3".to_string(),
            operation: AuthzOperation::ReleaseShare,
            http_path: None,
            dest_host: None,
            dest_port: None,
            worker_peer_id: Some("worker-3".to_string()),
            share_index: Some(1),
            delegate_peer_id: None,
            now_unix_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        let token = mint_release_share_token_b64(&worker_sk, &authz_ctx).unwrap();

        let (worker_kem_pub, _worker_kem_priv) = crypto::ensure_kem_keypair_on_disk().unwrap();
        let req = ShareRequest {
            manifest_id: manifest_id.to_string(),
            worker_peer_id: "worker-3".to_string(),
            node_cert_bytes: worker_cert.to_bytes(),
            assignment_sig: String::new(),
            assigned_at_secs: 0,
            share_index: Some(1),
            authz_token_b64: Some(token),
            worker_kem_pub: crypto::b64_encode(&worker_kem_pub),
            nonce: "n".to_string(),
            sig: String::new(),
        };

        let oracle = ShamirOracle {
            local_peer_id: local_peer_id.to_string(),
        };
        let resp = oracle.release_key_material(&req).await.expect("share release succeeds");
        assert_eq!(resp.manifest_id, manifest_id);
        assert!(!resp.wrapped_share.is_empty());
    }
}
