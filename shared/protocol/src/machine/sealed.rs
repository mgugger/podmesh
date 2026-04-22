//! Wire types for sealed workload submission (Shamir secret sharing).
//!
//! Trust model:
//!   podctl → GET /api/v1/custodians → receives custodian KEM pubkeys
//!   podctl → generates random DEK, encrypts spec, splits DEK into N Shamir shares
//!   podctl → POST /api/v1/workloads/submit → `WorkloadSubmission`
//!   Scheduler → validates signatures, distributes `WorkloadAssignmentV2` to custodians
//!   Scheduler → NEVER sees plaintext spec or raw DEK

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Custodian discovery (scheduler → client)
// ---------------------------------------------------------------------------

/// A single custodian node as returned by `GET /api/v1/custodians`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustodianInfo {
    /// libp2p PeerId (base58) of the custodian node.
    pub peer_id: String,
    /// Base64 X25519 KEM public key of the custodian node.
    pub kem_pubkey_b64: String,
}

/// Response from `GET /api/v1/custodians`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustodiansResponse {
    pub custodians: Vec<CustodianInfo>,
}

// ---------------------------------------------------------------------------
// Sealed spec (produced by podctl, never by scheduler)
// ---------------------------------------------------------------------------

/// Sealing version.
pub const SEAL_VERSION_V1: u8 = 1;

/// The encrypted, self-contained workload spec.
///
/// Produced entirely by `podctl` before submission. The scheduler stores and
/// forwards this without ever decrypting it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedSpec {
    /// Blake3 manifest ID (hex, 16 chars) — blake3(spec_json bytes)[..8].
    pub manifest_id: String,
    /// Base64 Ed25519 owner pubkey.
    pub owner_pubkey: String,
    /// XChaCha20-Poly1305 ciphertext of the spec JSON (DEK-encrypted).
    pub ciphertext: Vec<u8>,
    /// 24-byte XChaCha20 nonce.
    pub nonce: Vec<u8>,
    /// Total shares generated (n).
    pub kfrag_count: u8,
    /// Minimum shares needed to reconstruct (k).
    pub kfrag_threshold: u8,
    /// Unix epoch seconds when this spec was sealed by podctl.
    pub sealed_at_secs: u64,
    /// Sealing version — always `SEAL_VERSION_V1` (1).
    pub submission_version: u8,
    /// Number of worker replicas to deploy (default 1).
    #[serde(default = "default_replica_count")]
    pub replica_count: u8,
}

impl SealedSpec {
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("SealedSpec serialization ok")
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

// ---------------------------------------------------------------------------
// Client → Scheduler  (POST /api/v1/workloads/submit)
// ---------------------------------------------------------------------------

/// One custodian's DEK share, ECIES-wrapped to their KEM pubkey.
/// Produced by podctl, forwarded opaquely by the scheduler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmittedShare {
    /// libp2p PeerId of the custodian this share is addressed to.
    pub custodian_peer_id: String,
    /// ECIES-wrapped DEK share bytes (opaque to the scheduler).
    pub wrapped_bytes: Vec<u8>,
    /// 1-based share index within the full Shamir split.
    pub share_index: u8,
}

/// Submitted by `podctl` after sealing locally.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadSubmission {
    /// The fully sealed spec, produced by podctl.
    pub sealed_spec: SealedSpec,
    /// Required capabilities for worker scheduling (e.g. ["gpu", "region:eu"]).
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    /// Owner's Ed25519 signature (base64) over postcard(sealed_spec).
    pub submission_sig: String,
    /// Per-custodian ECIES-wrapped DEK shares, one per entry in `kfrag_count`.
    /// The scheduler forwards each entry to the corresponding custodian peer.
    #[serde(default)]
    pub wrapped_shares: Vec<SubmittedShare>,
    /// Number of worker replicas to deploy (default 1).
    #[serde(default = "default_replica_count")]
    pub replica_count: u8,
}

fn default_replica_count() -> u8 { 1 }

impl WorkloadSubmission {
    /// Verify the owner's submission signature over the sealed spec bytes.
    pub fn verify_submission_sig(&self) -> anyhow::Result<()> {
        let sig_bytes = crypto::b64_decode(&self.submission_sig)?;
        let pub_bytes = crypto::b64_decode(&self.sealed_spec.owner_pubkey)?;
        let sealed_bytes = self.sealed_spec.to_bytes();
        crypto::verify_envelope(&pub_bytes, &sealed_bytes, &sig_bytes)
    }
}

// ---------------------------------------------------------------------------
// Scheduler → Custodian  (via scheduler_rr)
// ---------------------------------------------------------------------------

/// Sent from the scheduler to each custodian after validating a `WorkloadSubmission`.
/// Contains the DEK share wrapped (ECIES) to this custodian's X25519 KEM pubkey.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadAssignmentV2 {
    /// The sealed spec.
    pub sealed_spec: SealedSpec,
    /// Full list of all custodian peer IDs.
    pub all_custodian_peers: Vec<String>,
    /// Required capabilities.
    pub required_capabilities: Vec<String>,
    /// Scheduler sig over postcard(sealed_spec bytes).
    pub scheduler_sig: String,
    /// The DEK share bytes wrapped (ECIES) to this custodian's X25519 KEM pubkey.
    pub wrapped_kfrag: Vec<u8>,
    /// 1-based index of this share within the full set.
    pub kfrag_index: u8,
    /// Base64 Ed25519 signing pubkey of the coordinator node.
    /// Custodians store this and use it to verify worker assignment tokens.
    #[serde(default)]
    pub coordinator_pubkey: String,
}

impl WorkloadAssignmentV2 {
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("WorkloadAssignmentV2 serialization ok")
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

// ---------------------------------------------------------------------------
// Coordinator → Worker  (via scheduler_rr)
// ---------------------------------------------------------------------------

/// Sent from the elected custodian-coordinator to a winning worker.
/// Contains the sealed spec, custodian list, and per-custodian DEK shares
/// already re-wrapped to the worker's KEM public key so the worker can unseal
/// without a round-trip back to custodians.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadDispatch {
    /// The sealed spec.
    pub sealed_spec: SealedSpec,
    /// Ordered list of all custodian peer IDs.
    pub custodian_peers: Vec<String>,
    /// Coordinator (custodian) Ed25519 sig over postcard(sealed_spec bytes).
    pub coordinator_sig: String,
    /// Per-custodian DEK shares, each re-wrapped to the worker's KEM public key.
    /// One entry per custodian (may be fewer if not all shares are available).
    #[serde(default)]
    pub worker_wrapped_shares: Vec<Vec<u8>>,
    /// Coordinator peer ID — the worker skips this custodian in share collection
    /// since the coordinator already pre-wrapped its own share in `worker_wrapped_shares`.
    #[serde(default)]
    pub coordinator_peer_id: String,
    /// Time-limited assignment token: coordinator Ed25519 sig over
    /// `manifest_id || worker_peer_id || assigned_at_secs (big-endian u64)`.
    /// Workers include this in every `ShareRequest` so custodians can verify
    /// they are talking to an authorised worker.
    #[serde(default)]
    pub assignment_token: String,
    /// Unix epoch seconds when this dispatch was issued. Custodians reject
    /// `ShareRequest`s where `now - assigned_at_secs > ASSIGNMENT_TOKEN_TTL_SECS`.
    #[serde(default)]
    pub assigned_at_secs: u64,
}

impl WorkloadDispatch {
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("WorkloadDispatch serialization ok")
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

// ---------------------------------------------------------------------------
// REST response
// ---------------------------------------------------------------------------

/// Response returned to podctl after a successful submission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadSubmissionResponse {
    pub manifest_id: String,
    /// Number of custodian nodes that acknowledged the assignment.
    pub custodians_assigned: usize,
    /// PeerIds of the assigned custodians (for the client's records).
    pub custodian_peers: Vec<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sealed_spec() -> SealedSpec {
        SealedSpec {
            manifest_id: "aabbccddeeff0011".to_string(),
            owner_pubkey: "BBBB".to_string(),
            ciphertext: vec![1, 2, 3, 4],
            nonce: vec![0u8; 24],
            kfrag_count: 3,
            kfrag_threshold: 2,
            sealed_at_secs: 1_700_000_000,
            submission_version: SEAL_VERSION_V1,
            replica_count: 1,
        }
    }

    #[test]
    fn test_sealed_spec_roundtrip_postcard() {
        let spec = make_sealed_spec();
        let bytes = spec.to_bytes();
        let spec2 = SealedSpec::from_bytes(&bytes).unwrap();
        assert_eq!(spec, spec2);
    }

    #[test]
    fn test_workload_submission_roundtrip_serde() {
        let sub = WorkloadSubmission {
            sealed_spec: make_sealed_spec(),
            required_capabilities: vec!["gpu".to_string()],
            submission_sig: "sig".to_string(),
            wrapped_shares: vec![],
            replica_count: 1,
        };
        let json = serde_json::to_string(&sub).unwrap();
        let sub2: WorkloadSubmission = serde_json::from_str(&json).unwrap();
        assert_eq!(sub, sub2);
    }

    #[test]
    fn test_workload_assignment_v2_roundtrip_postcard() {
        let assignment = WorkloadAssignmentV2 {
            sealed_spec: make_sealed_spec(),
            all_custodian_peers: vec!["p1".to_string(), "p2".to_string()],
            required_capabilities: vec!["gpu".to_string()],
            scheduler_sig: "csig".to_string(),
            wrapped_kfrag: vec![1, 2, 3],
            kfrag_index: 1,
            coordinator_pubkey: String::new(),
        };
        let bytes = assignment.to_bytes();
        let a2 = WorkloadAssignmentV2::from_bytes(&bytes).unwrap();
        assert_eq!(assignment, a2);
    }

    #[test]
    fn test_workload_dispatch_roundtrip_postcard() {
        let dispatch = WorkloadDispatch {
            sealed_spec: make_sealed_spec(),
            custodian_peers: vec!["p1".to_string(), "p2".to_string(), "p3".to_string()],
            coordinator_sig: "coord-sig".to_string(),
            worker_wrapped_shares: vec![],
            coordinator_peer_id: String::new(),
            assignment_token: String::new(),
            assigned_at_secs: 0,
        };
        let bytes = dispatch.to_bytes();
        let d2 = WorkloadDispatch::from_bytes(&bytes).unwrap();
        assert_eq!(dispatch, d2);
    }

    #[test]
    fn test_custodian_info_serde() {
        let info = CustodianInfo {
            peer_id: "12D3Koo...".to_string(),
            kem_pubkey_b64: "AAAA==".to_string(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let info2: CustodianInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, info2);
    }

    #[test]
    fn test_verify_submission_sig_roundtrip() {
        use crypto::{b64_encode, ensure_keypair_ephemeral, sign_data_with_key, set_keypair_config, KeypairConfig, KeypairMode};
        set_keypair_config(KeypairConfig {
            signing_mode: KeypairMode::Ephemeral,
            kem_mode: KeypairMode::Ephemeral,
            key_directory: None,
        });
        let (pk, sk) = ensure_keypair_ephemeral().unwrap();
        let mut spec = make_sealed_spec();
        spec.owner_pubkey = b64_encode(&pk);
        let sealed_bytes = spec.to_bytes();
        let sig = sign_data_with_key(&sk, &sealed_bytes).unwrap();
        let sub = WorkloadSubmission {
            sealed_spec: spec,
            required_capabilities: vec![],
            submission_sig: b64_encode(&sig),
            wrapped_shares: vec![],
            replica_count: 1,
        };
        sub.verify_submission_sig().unwrap();
    }
}
