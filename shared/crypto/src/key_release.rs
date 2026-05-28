//! KeyReleaseOracle trait — abstraction over Shamir (v1) and PRE (v2) key release mechanisms.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A request from a worker to a custodian for its share of the DEK.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareRequest {
    pub manifest_id: String,
    pub worker_peer_id: String,
    /// postcard-encoded NodeCert — custodian verifies role != Custodian
    pub node_cert_bytes: Vec<u8>,
    /// Coordinator Ed25519 sig over `manifest_id || worker_peer_id || assigned_at_secs`.
    /// Custodians verify this against the stored coordinator pubkey.
    pub assignment_sig: String,
    /// Unix epoch seconds when the coordinator issued the WorkloadDispatch.
    /// Custodians reject requests where `now - assigned_at_secs > ASSIGNMENT_TOKEN_TTL_SECS`.
    #[serde(default)]
    pub assigned_at_secs: u64,
    /// Optional 1-based share index expected by the requester.
    /// Used for strict binding of request intent to a specific custodian shard.
    #[serde(default)]
    pub share_index: Option<u32>,
    /// Optional base64url Biscuit authz token for share release / delegation flows.
    #[serde(default)]
    pub authz_token_b64: Option<String>,
    /// base64 X25519 pubkey — the share will be wrapped to this key
    pub worker_kem_pub: String,
    /// Random nonce for replay prevention
    pub nonce: String,
    /// Worker Ed25519 sig over all above fields (canonical postcard bytes)
    pub sig: String,
}

impl ShareRequest {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let canonical = ShareRequest { sig: String::new(), ..self.clone() };
        postcard::to_allocvec(&canonical).expect("ShareRequest serialization should succeed")
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("ShareRequest serialization should succeed")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

/// Response from a custodian containing a wrapped DEK share.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareResponse {
    pub manifest_id: String,
    /// The DEK share wrapped (encrypted) to worker_kem_pub — only the worker can decrypt.
    /// Format: version 0x03 recipient blob from crypto::encrypt_payload_for_recipient
    pub wrapped_share: Vec<u8>,
    /// Custodian Ed25519 sig over manifest_id + worker_peer_id + wrapped_share
    pub custodian_sig: String,
}

impl ShareResponse {
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("ShareResponse serialization should succeed")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

/// Abstraction over key release mechanisms.
/// v1: ShamirOracle — Shamir secret sharing
/// v2: PreOracle — Umbral proxy re-encryption (Phase 8)
#[async_trait]
pub trait KeyReleaseOracle: Send + Sync {
    /// Release key material to an eligible worker.
    /// Returns a wrapped share decryptable only by the worker's KEM privkey.
    async fn release_key_material(&self, request: &ShareRequest) -> anyhow::Result<ShareResponse>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockOracle;

    #[async_trait::async_trait]
    impl KeyReleaseOracle for MockOracle {
        async fn release_key_material(&self, req: &ShareRequest) -> anyhow::Result<ShareResponse> {
            Ok(ShareResponse {
                manifest_id: req.manifest_id.clone(),
                wrapped_share: vec![1, 2, 3],
                custodian_sig: "mock-sig".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn test_shamir_oracle_trait_object_usable() {
        let oracle: Box<dyn KeyReleaseOracle> = Box::new(MockOracle);
        let req = ShareRequest {
            manifest_id: "test".to_string(),
            worker_peer_id: "peer1".to_string(),
            node_cert_bytes: vec![],
            assignment_sig: "sig".to_string(),
            assigned_at_secs: 0,
            share_index: None,
            authz_token_b64: None,
            worker_kem_pub: "pub".to_string(),
            nonce: "nonce".to_string(),
            sig: "sig".to_string(),
        };
        let resp = oracle.release_key_material(&req).await.unwrap();
        assert_eq!(resp.manifest_id, "test");
    }
}
