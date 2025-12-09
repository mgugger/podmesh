//! Envelope parsing robustness tests.
//!
//! Tests that envelope parsing fails gracefully for malformed data:
//! - Truncated data
//! - Invalid postcard data
//! - Empty data
//! - Missing or malformed signature/pubkey fields

use podmesh_scheduler::podmesh_p2p::envelope::verify_envelope;
use protocol::machine::build_envelope_signed;
use std::time::Duration;

fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn test_truncated_data() {
    let truncated_data = b"truncated";
    let result = protocol::machine::root_as_envelope(truncated_data);
    assert!(
        result.is_err(),
        "Truncated data should not parse as envelope"
    );

    let verify_result = verify_envelope(truncated_data, Duration::from_secs(300));
    assert!(
        verify_result.is_err(),
        "Truncated envelope should fail verification"
    );
}

#[test]
fn test_invalid_postcard_data() {
    let invalid_data = b"this is definitely not a postcard envelope at all";
    let result = protocol::machine::root_as_envelope(invalid_data);
    assert!(result.is_err(), "Invalid data should not parse as envelope");

    let verify_result = verify_envelope(invalid_data, Duration::from_secs(300));
    assert!(
        verify_result.is_err(),
        "Invalid envelope should fail verification"
    );

    let error_msg = format!("{:?}", verify_result.unwrap_err());
    assert!(
        error_msg.contains("parse") || error_msg.contains("postcard"),
        "Error should mention parsing issue: {}",
        error_msg
    );
}

#[test]
fn test_empty_data() {
    let empty_data = b"";
    let result = protocol::machine::root_as_envelope(empty_data);
    assert!(result.is_err(), "Empty data should not parse as envelope");

    let verify_result = verify_envelope(empty_data, Duration::from_secs(300));
    assert!(
        verify_result.is_err(),
        "Empty envelope should fail verification"
    );
}

#[test]
fn test_empty_signature_field() {
    let envelope_with_empty_sig = build_envelope_signed(
        b"test payload",
        "test",
        "empty-sig-nonce",
        current_timestamp_ms(),
        "ed25519",
        "ed25519",
        "",
        "",
        None,
    );

    let result = verify_envelope(&envelope_with_empty_sig, Duration::from_secs(300));
    assert!(
        result.is_err(),
        "Envelope with empty signature should fail verification"
    );

    let error_msg = format!("{:?}", result.unwrap_err());
    assert!(
        error_msg.contains("base64")
            || error_msg.contains("decode")
            || error_msg.contains("signature"),
        "Error should mention missing signature data: {}",
        error_msg
    );
}

#[test]
fn test_malformed_base64_fields() {
    let malformed_b64_envelope = build_envelope_signed(
        b"test payload",
        "test",
        "malformed-b64-nonce",
        current_timestamp_ms(),
        "ed25519",
        "ed25519",
        "malformed-base64-!!!",
        "also-malformed-base64-@@@",
        None,
    );

    let result = verify_envelope(&malformed_b64_envelope, Duration::from_secs(300));
    assert!(
        result.is_err(),
        "Envelope with malformed base64 should fail verification"
    );

    let error_msg = format!("{:?}", result.unwrap_err());
    assert!(
        error_msg.contains("base64") || error_msg.contains("decode"),
        "Error should mention base64 decoding issue: {}",
        error_msg
    );
}
