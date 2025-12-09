//! Envelope nonce validation tests.
//!
//! Tests nonce-related validation:
//! - Replay protection (duplicate nonce rejection)
//! - Empty nonce handling

use crypto::{ensure_keypair_ephemeral, sign_envelope};
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
fn test_nonce_replay_protection() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");
    let payload = b"nonce validation test";

    let ts = current_timestamp_ms();
    let canonical_bytes = build_envelope_canonical(
        payload,
        "test",
        "duplicate-nonce-123",
        ts,
        "ed25519",
        None,
    );

    let (sig_b64, pub_b64) =
        sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign envelope");

    let envelope = build_envelope_signed(
        payload,
        "test",
        "duplicate-nonce-123",
        ts,
        "ed25519",
        "ed25519",
        &sig_b64,
        &pub_b64,
        None,
    );

    let first_result = verify_envelope(&envelope, Duration::from_secs(300));
    assert!(
        first_result.is_ok(),
        "First verification should succeed: {:?}",
        first_result.err()
    );

    let second_result = verify_envelope(&envelope, Duration::from_secs(300));
    assert!(
        second_result.is_err(),
        "Second verification should fail due to nonce replay"
    );

    let error_msg = format!("{:?}", second_result.unwrap_err());
    assert!(
        error_msg.contains("replay") || error_msg.contains("nonce"),
        "Error should mention replay or nonce issue: {}",
        error_msg
    );
}

#[test]
fn test_empty_nonce() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");
    let payload = b"nonce validation test";

    let ts = current_timestamp_ms();
    let canonical_empty_nonce =
        build_envelope_canonical(payload, "test", "", ts, "ed25519", None);

    let (sig_b64, pub_b64) = sign_envelope(&privb, &pubb, &canonical_empty_nonce)
        .expect("Failed to sign envelope with empty nonce");

    let envelope_empty_nonce = build_envelope_signed(
        payload,
        "test",
        "",
        ts,
        "ed25519",
        "ed25519",
        &sig_b64,
        &pub_b64,
        None,
    );

    let result = verify_envelope(&envelope_empty_nonce, Duration::from_secs(300));
    assert!(
        result.is_ok(),
        "Empty nonce should not prevent verification: {:?}",
        result.err()
    );
}
