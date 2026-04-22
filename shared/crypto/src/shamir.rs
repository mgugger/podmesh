//! Shamir secret sharing helpers for podmesh.
//!
//! Uses `vsss-rs` `Gf256::split_array` / `Gf256::combine_array` — constant-time
//! operations over GF(2^8), no elliptic curve dependency needed.
//!
//! # Sealing flow
//!
//! **Owner / podctl — `seal_shamir`:**
//! 1. Generate a random 32-byte DEK (XChaCha20-Poly1305 key).
//! 2. Encrypt spec JSON with DEK → `(ciphertext, nonce)`.
//! 3. Split DEK into N Shamir shares (threshold K) over GF(256).
//! 4. ECIES-wrap each share to the corresponding custodian's X25519 KEM pubkey.
//!
//! **Custodian — oracle (`ShamirOracle`):**
//! - Unwrap the stored share with own KEM privkey.
//! - Re-wrap it to the requesting worker's KEM pubkey.
//! - Return in `ShareResponse.wrapped_share`.
//!
//! **Worker — `unseal_shamir`:**
//! - Unwrap each share with own KEM privkey.
//! - Combine shares → DEK.
//! - Decrypt ciphertext with DEK.

use anyhow::Context;
use vsss_rs::Gf256;
use zeroize::Zeroizing;

use crate::{
    decrypt_payload_from_recipient_blob,
    encrypt_payload_for_recipient,
};

/// Output of `seal_shamir`.
pub struct ShamirSealOutput {
    /// XChaCha20-Poly1305 ciphertext of the spec JSON.
    pub ciphertext: Vec<u8>,
    /// 24-byte nonce used for the AEAD encryption.
    pub nonce: Vec<u8>,
    /// One `WrappedShare` per custodian.
    pub wrapped_shares: Vec<WrappedShare>,
}

/// A single DEK share wrapped (ECIES) to a custodian's X25519 KEM pubkey.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WrappedShare {
    /// 1-based share index (mirrors the identifier byte inside the share).
    pub index: u8,
    /// libp2p PeerId of the custodian.
    pub custodian_peer_id: String,
    /// Base64 X25519 KEM pubkey of the custodian — for reference.
    pub custodian_kem_pubkey: String,
    /// ECIES-wrapped share bytes (version 0x03 blob).
    pub wrapped_bytes: Vec<u8>,
}

/// Encrypt `spec_json` with a fresh random DEK and split it into N Shamir shares.
///
/// # Arguments
/// * `spec_json`  — plaintext workload spec
/// * `custodians` — `(peer_id, kem_pub_bytes)` per custodian, length = N
/// * `threshold`  — minimum shares needed to reconstruct (K)
pub fn seal_shamir(
    spec_json: &str,
    custodians: &[(String, Vec<u8>)],
    threshold: usize,
) -> anyhow::Result<ShamirSealOutput> {
    let n = custodians.len();
    anyhow::ensure!(n >= threshold, "n={n} must be >= threshold={threshold}");
    anyhow::ensure!(threshold >= 1, "threshold must be >= 1");
    anyhow::ensure!(n <= 255, "n must fit in u8 (max 255 custodians)");

    // Generate a random 32-byte DEK.
    let mut dek = Zeroizing::new([0u8; 32]);
    {
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut dek[..]);
    }

    // Encrypt spec_json with the DEK (XChaCha20-Poly1305).
    let (ciphertext, nonce) = {
        use chacha20poly1305::{XChaCha20Poly1305, XNonce, aead::{Aead, KeyInit}};
        use rand::RngCore;
        let cipher = XChaCha20Poly1305::new_from_slice(&dek[..])
            .map_err(|e| anyhow::anyhow!("cipher init: {e}"))?;
        let mut nonce_bytes = [0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from(nonce_bytes);
        let ciphertext = cipher
            .encrypt(&nonce, spec_json.as_bytes())
            .map_err(|e| anyhow::anyhow!("encrypt: {e}"))?;
        (ciphertext, nonce_bytes.to_vec())
    };

    // Split the DEK into N Shamir shares over GF(256).
    // Each share is a Vec<u8> where share[0] is the participant identifier (1-based).
    //
    // vsss-rs requires threshold >= 2. For the degenerate n=1 / threshold=1 case
    // (single custodian), we skip the split and use the raw DEK bytes as the one
    // "share" (index = 1). This is semantically equivalent to 1-of-1 Shamir.
    let shares: Vec<Vec<u8>> = if n == 1 {
        // Degenerate case: single custodian, no actual splitting needed.
        // Prepend index byte (1) to match the normal share format.
        let mut single = vec![1u8]; // index = 1
        single.extend_from_slice(&dek[..]);
        vec![single]
    } else {
        Gf256::split_array(threshold, n, &dek[..], rand::rngs::OsRng)
            .map_err(|e| anyhow::anyhow!("Shamir split failed: {e:?}"))?
    };

    // Wrap each share to the corresponding custodian's KEM pubkey.
    let mut wrapped_shares = Vec::with_capacity(n);
    for (i, (share_bytes, (peer_id, custodian_kem_pub))) in
        shares.iter().zip(custodians.iter()).enumerate()
    {
        let index = share_bytes[0]; // first byte is the participant identifier
        let wrapped = encrypt_payload_for_recipient(custodian_kem_pub, share_bytes)
            .with_context(|| format!("wrap share {i} to custodian KEM pubkey"))?;

        wrapped_shares.push(WrappedShare {
            index,
            custodian_peer_id: peer_id.clone(),
            custodian_kem_pubkey: crate::b64_encode(custodian_kem_pub),
            wrapped_bytes: wrapped,
        });
    }

    Ok(ShamirSealOutput {
        ciphertext,
        nonce,
        wrapped_shares,
    })
}

/// Unwrap shares, combine them to reconstruct the DEK, and decrypt the ciphertext.
///
/// # Arguments
/// * `ciphertext`         — AEAD ciphertext from `ShamirSealOutput`
/// * `nonce`              — 24-byte nonce from `ShamirSealOutput`
/// * `wrapped_shares`     — each ECIES-wrapped to `worker_kem_priv_bytes`
/// * `worker_kem_priv_bytes` — worker's X25519 KEM private key
pub fn unseal_shamir(
    ciphertext: &[u8],
    nonce: &[u8],
    wrapped_shares: &[Vec<u8>],
    worker_kem_priv_bytes: &[u8],
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(!wrapped_shares.is_empty(), "no shares provided");

    // Unwrap each share.
    let mut raw_shares: Vec<Vec<u8>> = Vec::with_capacity(wrapped_shares.len());
    for (i, wrapped) in wrapped_shares.iter().enumerate() {
        let share_bytes = decrypt_payload_from_recipient_blob(wrapped, worker_kem_priv_bytes)
            .with_context(|| format!("unwrap share {}", i + 1))?;
        raw_shares.push(share_bytes);
    }

    // Reconstruct the DEK.
    // For the degenerate n=1 case, the single share IS the DEK (prepended with index byte).
    let dek_bytes = if raw_shares.len() == 1 && raw_shares[0].len() == 33 && raw_shares[0][0] == 1 {
        // Single-custodian path: strip the index byte to recover the raw DEK.
        raw_shares[0][1..].to_vec()
    } else {
        Gf256::combine_array(&raw_shares)
            .map_err(|e| anyhow::anyhow!("Shamir combine failed: {e:?}"))?
    };
    let dek: [u8; 32] = dek_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("recovered DEK has wrong length: {}", dek_bytes.len()))?;
    let dek = Zeroizing::new(dek);

    // Decrypt spec.
    let plaintext = crate::decrypt_manifest(&dek, nonce, ciphertext)
        .context("decrypt spec with recovered DEK")?;

    Ok(plaintext)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use x25519_dalek::{PublicKey as X25519Pub, StaticSecret};

    fn make_custodian() -> (String, Vec<u8>, Vec<u8>) {
        let sk = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let pk = X25519Pub::from(&sk);
        let pid = format!("peer-{}", &crate::b64_encode(pk.as_bytes())[..6]);
        (pid, pk.as_bytes().to_vec(), sk.as_bytes().to_vec())
    }

    fn simulate_custodian_reencrypt(
        wrapped_share: &[u8],
        custodian_kem_priv: &[u8],
        worker_kem_pub: &[u8],
    ) -> Vec<u8> {
        let raw = decrypt_payload_from_recipient_blob(wrapped_share, custodian_kem_priv)
            .expect("custodian unwrap");
        encrypt_payload_for_recipient(worker_kem_pub, &raw)
            .expect("custodian re-wrap to worker")
    }

    #[test]
    fn test_seal_unseal_2_of_3() {
        let spec = r#"{"name":"myapp","image":"alpine:3"}"#;
        let custodians: Vec<_> = (0..3).map(|_| make_custodian()).collect();
        let inputs: Vec<(String, Vec<u8>)> = custodians
            .iter()
            .map(|(pid, pub_b, _)| (pid.clone(), pub_b.clone()))
            .collect();

        let sealed = seal_shamir(spec, &inputs, 2).expect("seal_shamir");
        assert_eq!(sealed.wrapped_shares.len(), 3);

        // Worker KEM pair
        let worker_sk = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let worker_pk = X25519Pub::from(&worker_sk);
        let worker_kem_priv = worker_sk.as_bytes().to_vec();
        let worker_kem_pub = worker_pk.as_bytes().to_vec();

        // Use 2 of 3 custodians (indices 0 and 2)
        let mut wrapped_for_worker = Vec::new();
        for i in [0usize, 2] {
            let w = simulate_custodian_reencrypt(
                &sealed.wrapped_shares[i].wrapped_bytes,
                &custodians[i].2,
                &worker_kem_pub,
            );
            wrapped_for_worker.push(w);
        }

        let plaintext = unseal_shamir(
            &sealed.ciphertext,
            &sealed.nonce,
            &wrapped_for_worker,
            &worker_kem_priv,
        ).expect("unseal_shamir");

        assert_eq!(plaintext.as_slice(), spec.as_bytes());
    }

    #[test]
    fn test_seal_unseal_3_of_5() {
        let spec = r#"{"kind":"Pod","name":"stress-test"}"#;
        let custodians: Vec<_> = (0..5).map(|_| make_custodian()).collect();
        let inputs: Vec<(String, Vec<u8>)> = custodians
            .iter()
            .map(|(pid, pub_b, _)| (pid.clone(), pub_b.clone()))
            .collect();

        let sealed = seal_shamir(spec, &inputs, 3).expect("seal_shamir 3-of-5");
        assert_eq!(sealed.wrapped_shares.len(), 5);

        let worker_sk = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let worker_pk = X25519Pub::from(&worker_sk);
        let worker_kem_priv = worker_sk.as_bytes().to_vec();
        let worker_kem_pub = worker_pk.as_bytes().to_vec();

        // Use shares 1, 3, 4
        let mut wrapped_for_worker = Vec::new();
        for i in [1usize, 3, 4] {
            let w = simulate_custodian_reencrypt(
                &sealed.wrapped_shares[i].wrapped_bytes,
                &custodians[i].2,
                &worker_kem_pub,
            );
            wrapped_for_worker.push(w);
        }

        let plaintext = unseal_shamir(
            &sealed.ciphertext,
            &sealed.nonce,
            &wrapped_for_worker,
            &worker_kem_priv,
        ).unwrap();
        assert_eq!(plaintext.as_slice(), spec.as_bytes());
    }

    #[test]
    fn test_insufficient_shares_fails() {
        let spec = b"secret spec";
        let custodians: Vec<_> = (0..3).map(|_| make_custodian()).collect();
        let inputs: Vec<(String, Vec<u8>)> = custodians
            .iter()
            .map(|(pid, pub_b, _)| (pid.clone(), pub_b.clone()))
            .collect();

        let sealed = seal_shamir(
            std::str::from_utf8(spec).unwrap(),
            &inputs,
            2,
        ).unwrap();

        let worker_sk = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let worker_pk = X25519Pub::from(&worker_sk);
        let worker_kem_priv = worker_sk.as_bytes().to_vec();
        let worker_kem_pub = worker_pk.as_bytes().to_vec();

        // Only 1 share — threshold is 2, should fail or return wrong plaintext
        let w = simulate_custodian_reencrypt(
            &sealed.wrapped_shares[0].wrapped_bytes,
            &custodians[0].2,
            &worker_kem_pub,
        );
        let result = unseal_shamir(&sealed.ciphertext, &sealed.nonce, &[w], &worker_kem_priv);
        // Either fails to combine or produces wrong DEK → AEAD auth failure
        assert!(result.is_err(), "1-of-2 threshold must not succeed");
    }
}
