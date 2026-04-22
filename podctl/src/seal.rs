//! Client-side workload sealing for podctl (Shamir secret sharing).
//!
//! The scheduler never sees the plaintext spec.
//!
//! # Sealing Flow
//! 1. podctl calls `GET /api/v1/custodians` → receives `Vec<CustodianInfo>`
//! 2. podctl calls `seal_workload(spec_json, custodians, owner_ed25519, n, k)`
//! 3. podctl calls `POST /api/v1/workloads/submit` with the resulting `WorkloadSubmission`

use anyhow::Context;
use std::time::{SystemTime, UNIX_EPOCH};

use crypto::{b64_decode, b64_encode, sign_data_with_key};
use protocol::machine::{CustodianInfo, SealedSpec, SubmittedShare, WorkloadSubmission, SEAL_VERSION_V1};

/// Seal a plaintext workload spec using Shamir secret sharing and build a `WorkloadSubmission`.
///
/// Returns the `WorkloadSubmission` and the per-custodian `WrappedShare` list so the
/// caller can send each wrapped share to the appropriate custodian via `WorkloadAssignmentV2`.
///
/// # Arguments
/// * `spec_json`            — plaintext workload spec
/// * `custodians`           — custodian list from `GET /api/v1/custodians`
/// * `owner_ed25519_pk`     — owner's Ed25519 pubkey (for `SealedSpec.owner_pubkey`)
/// * `owner_ed25519_sk`     — owner's Ed25519 secret key (for submission sig)
/// * `n`                    — number of custodians / shares
/// * `k`                    — reconstruction threshold
/// * `required_capabilities` — forwarded to workers during scheduling
#[allow(clippy::too_many_arguments)]
pub fn seal_workload(
    spec_json: &str,
    custodians: &[CustodianInfo],
    owner_ed25519_pk: &[u8],
    owner_ed25519_sk: &[u8],
    n: u8,
    k: u8,
    required_capabilities: Vec<String>,
) -> anyhow::Result<(WorkloadSubmission, Vec<crypto::WrappedShare>)> {
    anyhow::ensure!(!spec_json.is_empty(), "spec_json must not be empty");
    anyhow::ensure!(k >= 1, "threshold k must be >= 1");
    anyhow::ensure!(n >= k, "total shares n must be >= threshold k");
    anyhow::ensure!(
        custodians.len() >= n as usize,
        "need {} custodians but only {} provided",
        n,
        custodians.len()
    );

    // Build custodian input list: (peer_id, kem_pub_bytes)
    let custodian_inputs: Vec<(String, Vec<u8>)> = custodians[..n as usize]
        .iter()
        .map(|c| {
            let kem_bytes = b64_decode(&c.kem_pubkey_b64)
                .with_context(|| format!("invalid kem pubkey for {}", c.peer_id))?;
            Ok((c.peer_id.clone(), kem_bytes))
        })
        .collect::<anyhow::Result<_>>()?;

    // Seal using Shamir secret sharing
    let sealed_out = crypto::seal_shamir(spec_json, &custodian_inputs, k as usize)
        .context("seal_shamir failed")?;

    let manifest_id = {
        let hash = blake3::hash(spec_json.as_bytes());
        hash.as_bytes()[..8]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>()
    };

    let sealed_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let sealed_spec = SealedSpec {
        manifest_id,
        owner_pubkey: b64_encode(owner_ed25519_pk),
        ciphertext: sealed_out.ciphertext,
        nonce: sealed_out.nonce,
        kfrag_count: n,
        kfrag_threshold: k,
        sealed_at_secs,
        submission_version: SEAL_VERSION_V1,
        replica_count: parse_replica_count(spec_json),
    };

    let sealed_bytes = sealed_spec.to_bytes();
    let submission_sig_bytes = sign_data_with_key(owner_ed25519_sk, &sealed_bytes)
        .context("signing sealed_spec")?;

    // Build per-custodian SubmittedShare entries from the sealed output.
    let submitted_shares: Vec<SubmittedShare> = sealed_out
        .wrapped_shares
        .into_iter()
        .enumerate()
        .map(|(i, ws)| SubmittedShare {
            custodian_peer_id: ws.custodian_peer_id,
            wrapped_bytes: ws.wrapped_bytes,
            share_index: (i + 1) as u8,
        })
        .collect();

    let submission = WorkloadSubmission {
        sealed_spec,
        required_capabilities,
        submission_sig: b64_encode(&submission_sig_bytes),
        wrapped_shares: submitted_shares,
        replica_count: parse_replica_count(spec_json),
    };

    Ok((submission, vec![]))
}

/// Parse `spec.replicas` from a JSON spec, returning 1 if not found or invalid.
fn parse_replica_count(spec_json: &str) -> u8 {
    serde_json::from_str::<serde_json::Value>(spec_json)
        .ok()
        .and_then(|v| v.get("spec").and_then(|s| s.get("replicas")).and_then(|r| r.as_u64()))
        .map(|r| r.min(255) as u8)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use x25519_dalek::{PublicKey as X25519Pub, StaticSecret};

    fn make_custodian() -> (CustodianInfo, Vec<u8>) {
        let priv_key = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let pub_key = X25519Pub::from(&priv_key);
        let kem_pub_b64 = b64_encode(pub_key.as_bytes());
        (
            CustodianInfo {
                peer_id: format!("peer-{}", &kem_pub_b64[..6]),
                kem_pubkey_b64: kem_pub_b64,
            },
            priv_key.as_bytes().to_vec(),
        )
    }

    fn make_owner_ed25519() -> (Vec<u8>, Vec<u8>) {
        use ed25519_dalek::SigningKey;
        let sk = SigningKey::generate(&mut rand::rngs::OsRng);
        (sk.verifying_key().to_bytes().to_vec(), sk.to_bytes().to_vec())
    }

    #[test]
    fn test_seal_produces_valid_submission() {
        let (owner_ed_pk, owner_ed_sk) = make_owner_ed25519();
        let custodians_with_privs: Vec<_> = (0..3).map(|_| make_custodian()).collect();
        let custodians: Vec<_> = custodians_with_privs.iter().map(|(c, _)| c.clone()).collect();

        let spec = r#"{"name":"myapp","image":"alpine:3"}"#;
        let (sub, wrapped_shares) = seal_workload(
            spec, &custodians,
            &owner_ed_pk, &owner_ed_sk,
            3, 2, vec![],
        ).unwrap();

        sub.verify_submission_sig().unwrap();
        assert_eq!(sub.sealed_spec.submission_version, SEAL_VERSION_V1);
        assert_eq!(sub.wrapped_shares.len(), 3);
        assert_eq!(sub.sealed_spec.kfrag_count, 3);
        assert_eq!(sub.sealed_spec.kfrag_threshold, 2);
    }

    #[test]
    fn test_seal_and_unseal_roundtrip() {
        let (owner_ed_pk, owner_ed_sk) = make_owner_ed25519();
        let custodians_with_privs: Vec<_> = (0..3).map(|_| make_custodian()).collect();
        let custodians: Vec<_> = custodians_with_privs.iter().map(|(c, _)| c.clone()).collect();
        let custodian_privs: Vec<_> = custodians_with_privs.iter().map(|(_, p)| p.clone()).collect();

        // Worker KEM pair
        let worker_sk = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let worker_pk = X25519Pub::from(&worker_sk);
        let worker_kem_priv = worker_sk.as_bytes().to_vec();
        let worker_kem_pub = worker_pk.as_bytes().to_vec();

        let spec = r#"{"name":"myapp","image":"alpine:3"}"#;
        let (sub, _) = seal_workload(
            spec, &custodians,
            &owner_ed_pk, &owner_ed_sk,
            3, 2, vec![],
        ).unwrap();

        // Simulate 2 of 3 custodians re-wrapping their share to the worker's KEM pubkey.
        // Shares are now embedded in sub.wrapped_shares.
        let mut wrapped_for_worker = Vec::new();
        for i in [0usize, 2] {
            let raw = crypto::decrypt_payload_from_recipient_blob(
                &sub.wrapped_shares[i].wrapped_bytes,
                &custodian_privs[i],
            ).unwrap();
            let re_wrapped = crypto::encrypt_payload_for_recipient(&worker_kem_pub, &raw).unwrap();
            wrapped_for_worker.push(re_wrapped);
        }

        let plaintext = crypto::unseal_shamir(
            &sub.sealed_spec.ciphertext,
            &sub.sealed_spec.nonce,
            &wrapped_for_worker,
            &worker_kem_priv,
        ).unwrap();
        assert_eq!(plaintext.as_slice(), spec.as_bytes());
    }

    #[test]
    fn test_insufficient_custodians_rejected() {
        let (owner_ed_pk, owner_ed_sk) = make_owner_ed25519();
        let custodians: Vec<_> = (0..2).map(|_| make_custodian().0).collect();
        let err = seal_workload("spec", &custodians,
            &owner_ed_pk, &owner_ed_sk, 5, 3, vec![]);
        assert!(err.is_err());
    }
}
