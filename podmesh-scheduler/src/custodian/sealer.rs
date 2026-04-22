//! Worker-side workload unsealing (Shamir secret sharing).
//!
//! Sealing is done exclusively client-side in `podctl/src/seal.rs` — this module
//! handles decryption on the worker side after DEK shares have been collected.
//!
//! - `CustodianCandidate` — used by the custodian discovery flow
//! - `unseal_spec`        — collect wrapped DEK shares, reconstruct DEK, decrypt spec

use anyhow::Context;
use protocol::machine::SealedSpec;

/// A resolved custodian candidate: peer ID + KEM public key.
/// Used during custodian discovery (GET /api/v1/custodians and seal:* queries).
#[derive(Debug, Clone)]
pub struct CustodianCandidate {
    pub peer_id: String,
    pub kem_pubkey_b64: String,
}

/// Decrypt and recover the spec from a `SealedSpec` (Shamir secret sharing).
///
/// `wrapped_shares` are DEK shares ECIES-wrapped to the worker's KEM pubkey,
/// collected from k custodians. The worker unwraps and combines them to get the DEK.
pub fn unseal_spec(
    sealed: &SealedSpec,
    wrapped_shares: &[Vec<u8>],
    worker_kem_priv_bytes: &[u8],
) -> anyhow::Result<String> {
    let plaintext_bytes = crypto::unseal_shamir(
        &sealed.ciphertext,
        &sealed.nonce,
        wrapped_shares,
        worker_kem_priv_bytes,
    )
    .context("unseal_shamir failed")?;

    String::from_utf8(plaintext_bytes).context("spec is not valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto::{b64_encode, shamir::{seal_shamir, WrappedShare}};
    use protocol::machine::{SealedSpec, SEAL_VERSION_V1};
    use x25519_dalek::{PublicKey as X25519Pub, StaticSecret};

    #[test]
    fn test_unseal_spec_roundtrip() {
        let spec = r#"{"name":"test-v1","image":"alpine"}"#;

        let custodians: Vec<(String, Vec<u8>, Vec<u8>)> = (0..3).map(|i| {
            let s = StaticSecret::random_from_rng(rand::rngs::OsRng);
            let p = X25519Pub::from(&s);
            (format!("cust-{}", i), p.as_bytes().to_vec(), s.as_bytes().to_vec())
        }).collect();
        let cust_inputs: Vec<(String, Vec<u8>)> = custodians.iter()
            .map(|(id, pub_b, _)| (id.clone(), pub_b.clone()))
            .collect();

        let sealed_out = seal_shamir(spec, &cust_inputs, 2).unwrap();

        let sealed_spec = SealedSpec {
            manifest_id: "test-id".to_string(),
            owner_pubkey: b64_encode(&[0u8; 32]),
            ciphertext: sealed_out.ciphertext.clone(),
            nonce: sealed_out.nonce.clone(),
            kfrag_count: 3,
            kfrag_threshold: 2,
            sealed_at_secs: 0,
            submission_version: SEAL_VERSION_V1,
            replica_count: 1,
        };

        // Worker KEM pair
        let worker_sk = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let worker_pk = X25519Pub::from(&worker_sk);
        let worker_kem_priv = worker_sk.as_bytes().to_vec();
        let worker_kem_pub = worker_pk.as_bytes().to_vec();

        // 2 of 3 custodians re-wrap their share to the worker's KEM pubkey
        let mut wrapped_for_worker = Vec::new();
        for i in [0usize, 2] {
            let raw = crypto::decrypt_payload_from_recipient_blob(
                &sealed_out.wrapped_shares[i].wrapped_bytes,
                &custodians[i].2,
            ).unwrap();
            let re_wrapped = crypto::encrypt_payload_for_recipient(&worker_kem_pub, &raw).unwrap();
            wrapped_for_worker.push(re_wrapped);
        }

        let recovered = unseal_spec(&sealed_spec, &wrapped_for_worker, &worker_kem_priv).unwrap();
        assert_eq!(recovered, spec);
    }

    #[test]
    fn test_custodian_candidate_fields() {
        let c = CustodianCandidate {
            peer_id: "peer-1".to_string(),
            kem_pubkey_b64: "pubkey==".to_string(),
        };
        assert_eq!(c.peer_id, "peer-1");
        assert_eq!(c.kem_pubkey_b64, "pubkey==");
    }
}
