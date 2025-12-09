//! Envelope signature verification error tests.
//!
//! Tests that signature verification fails appropriately for various invalid scenarios:
//! - Invalid base64 signatures
//! - Invalid signature lengths  
//! - Wrong public keys
//! - Tampered payloads
//! - Invalid public key formats

use crypto::{b64_encode, ensure_keypair_ephemeral, sign_envelope};
use podmesh_scheduler::podmesh_p2p::envelope::verify_envelope;
use protocol::machine::{build_envelope_canonical, build_envelope_signed};
use std::time::Duration;

fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn test_invalid_base64_signature() {
    let (pubb, _privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");
    let payload = b"error handling test payload";

    let invalid_sig = "not-valid-base64!@#$%";
    let pub_b64 = b64_encode(&pubb);

    let invalid_sig_envelope = build_envelope_signed(
        payload,
        "test",
        "invalid-sig-nonce",
        current_timestamp_ms(),
        "ed25519",
        "ed25519",
        invalid_sig,
        &pub_b64,
        None,
    );

    let result = verify_envelope(&invalid_sig_envelope, Duration::from_secs(300));
    assert!(result.is_err(), "Invalid signature should fail");

    let error_msg = format!("{:?}", result.unwrap_err());
    assert!(
        error_msg.contains("base64") || error_msg.contains("decode"),
        "Error should mention base64 decoding issue: {}",
        error_msg
    );
}

#[test]
fn test_invalid_signature_length() {
    let (pubb, _privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");
    let payload = b"error handling test payload";

    let invalid_sig = b64_encode(b"invalid signature bytes that are too short");
    let pub_b64 = b64_encode(&pubb);

    let invalid_length_envelope = build_envelope_signed(
        payload,
        "test",
        "invalid-length-nonce",
        current_timestamp_ms(),
        "ed25519",
        "ed25519",
        &invalid_sig,
        &pub_b64,
        None,
    );

    let result = verify_envelope(&invalid_length_envelope, Duration::from_secs(300));
    assert!(result.is_err(), "Invalid signature length should fail");

    let error_msg = format!("{:?}", result.unwrap_err());
    assert!(
        error_msg.contains("signature") || error_msg.contains("invalid"),
        "Error should mention signature issue: {}",
        error_msg
    );
}

#[test]
fn test_wrong_public_key() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");
    let (pubb2, _privb2) = ensure_keypair_ephemeral().expect("Failed to generate second keypair");
    let payload = b"error handling test payload";

    let canonical_bytes = build_envelope_canonical(
        payload,
        "test",
        "error-test-nonce",
        current_timestamp_ms(),
        "ed25519",
        None,
    );

    let (sig_b64, _pub_b64) = sign_envelope(&privb, &pubb, &canonical_bytes)
        .expect("Failed to sign with correct keypair");
    let wrong_pub_b64 = b64_encode(&pubb2);

    let wrong_key_envelope = build_envelope_signed(
        payload,
        "test",
        "wrong-key-nonce",
        current_timestamp_ms(),
        "ed25519",
        "ed25519",
        &sig_b64,
        &wrong_pub_b64,
        None,
    );

    let result = verify_envelope(&wrong_key_envelope, Duration::from_secs(300));
    assert!(result.is_err(), "Wrong public key should fail verification");

    let error_msg = format!("{:?}", result.unwrap_err());
    assert!(
        error_msg.contains("verification") || error_msg.contains("failed"),
        "Error should mention verification failure: {}",
        error_msg
    );
}

#[test]
fn test_tampered_payload() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let original_payload = b"original payload for signing";
    let tampered_payload = b"tampered payload content";

    let original_canonical = build_envelope_canonical(
        original_payload,
        "test",
        "tamper-nonce",
        current_timestamp_ms(),
        "ed25519",
        None,
    );

    let (sig_b64, pub_b64) = sign_envelope(&privb, &pubb, &original_canonical)
        .expect("Failed to sign original payload");

    let tampered_envelope = build_envelope_signed(
        tampered_payload,
        "test",
        "tamper-nonce",
        current_timestamp_ms(),
        "ed25519",
        "ed25519",
        &sig_b64,
        &pub_b64,
        None,
    );

    let result = verify_envelope(&tampered_envelope, Duration::from_secs(300));
    assert!(result.is_err(), "Tampered payload should fail verification");

    let error_msg = format!("{:?}", result.unwrap_err());
    assert!(
        error_msg.contains("verification") || error_msg.contains("signature"),
        "Error should indicate signature verification failure: {}",
        error_msg
    );
}

#[test]
fn test_invalid_public_key_format() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");
    let payload = b"error handling test payload";

    let canonical_bytes = build_envelope_canonical(
        payload,
        "test",
        "error-test-nonce",
        current_timestamp_ms(),
        "ed25519",
        None,
    );

    let (sig_b64, _) =
        sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign envelope");
    let invalid_pub = b64_encode(b"invalid public key bytes");

    let invalid_pub_envelope = build_envelope_signed(
        payload,
        "test",
        "invalid-pub-nonce",
        current_timestamp_ms(),
        "ed25519",
        "ed25519",
        &sig_b64,
        &invalid_pub,
        None,
    );

    let result = verify_envelope(&invalid_pub_envelope, Duration::from_secs(300));
    assert!(
        result.is_err(),
        "Invalid public key should fail verification"
    );

    let error_msg = format!("{:?}", result.unwrap_err());
    assert!(
        error_msg.contains("key") || error_msg.contains("bytes"),
        "Error should mention key/bytes issue: {}",
        error_msg
    );
}
