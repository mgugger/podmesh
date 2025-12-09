//! Envelope timestamp edge case tests.
//!
//! Tests timestamp validation:
//! - Zero timestamp (should fail drift check)
//! - Maximum timestamp (should fail drift check)
//! - Current timestamp (should pass)
//! - Algorithm mismatch handling

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
fn test_zero_timestamp() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");
    let payload = b"timestamp test payload";

    let canonical_zero_ts =
        build_envelope_canonical(payload, "test", "zero-ts-nonce", 0, "ed25519", None);

    let (sig_b64, pub_b64) = sign_envelope(&privb, &pubb, &canonical_zero_ts)
        .expect("Failed to sign envelope with zero timestamp");

    let envelope_zero_ts = build_envelope_signed(
        payload,
        "test",
        "zero-ts-nonce",
        0,
        "ed25519",
        "ed25519",
        &sig_b64,
        &pub_b64,
        None,
    );

    let result = verify_envelope(&envelope_zero_ts, Duration::from_secs(300));
    assert!(
        result.is_err(),
        "Zero timestamp should fail drift validation"
    );
    let error_msg = format!("{:?}", result.unwrap_err());
    assert!(
        error_msg.contains("drift"),
        "Error should mention drift: {}",
        error_msg
    );
}

#[test]
fn test_max_timestamp() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");
    let payload = b"timestamp test payload";

    let max_timestamp = u64::MAX;
    let canonical_max_ts = build_envelope_canonical(
        payload,
        "test",
        "max-ts-nonce",
        max_timestamp,
        "ed25519",
        None,
    );

    let (sig_b64, pub_b64) = sign_envelope(&privb, &pubb, &canonical_max_ts)
        .expect("Failed to sign envelope with max timestamp");

    let envelope_max_ts = build_envelope_signed(
        payload,
        "test",
        "max-ts-nonce",
        max_timestamp,
        "ed25519",
        "ed25519",
        &sig_b64,
        &pub_b64,
        None,
    );

    let result = verify_envelope(&envelope_max_ts, Duration::from_secs(300));
    assert!(
        result.is_err(),
        "Maximum timestamp should fail drift validation"
    );
    let error_msg = format!("{:?}", result.unwrap_err());
    assert!(
        error_msg.contains("drift"),
        "Error should mention drift: {}",
        error_msg
    );
}

#[test]
fn test_current_timestamp() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");
    let payload = b"timestamp test payload";

    let current_ts = current_timestamp_ms();
    let nonce = format!("current-ts-nonce-{}", current_ts);
    let canonical_current_ts =
        build_envelope_canonical(payload, "test", &nonce, current_ts, "ed25519", None);

    let (sig_b64, pub_b64) = sign_envelope(&privb, &pubb, &canonical_current_ts)
        .expect("Failed to sign envelope with current timestamp");

    let envelope_current_ts = build_envelope_signed(
        payload,
        "test",
        &nonce,
        current_ts,
        "ed25519",
        "ed25519",
        &sig_b64,
        &pub_b64,
        None,
    );

    let result = verify_envelope(&envelope_current_ts, Duration::from_secs(300));
    assert!(
        result.is_ok(),
        "Current timestamp should verify: {:?}",
        result.err()
    );
}

#[test]
fn test_algorithm_mismatch() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");
    let payload = b"algorithm test payload";

    let ts = current_timestamp_ms();
    let canonical_bytes = build_envelope_canonical(
        payload,
        "test",
        "alg-test-nonce",
        ts,
        "different-algorithm",
        None,
    );

    let (sig_b64, pub_b64) =
        sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign envelope");

    let envelope = build_envelope_signed(
        payload,
        "test",
        "alg-test-nonce",
        ts,
        "different-algorithm",
        "ed25519",
        &sig_b64,
        &pub_b64,
        None,
    );

    let result = verify_envelope(&envelope, Duration::from_secs(300));
    assert!(
        result.is_ok(),
        "Different algorithm string should still verify (canonical consistency): {:?}",
        result.err()
    );
}
