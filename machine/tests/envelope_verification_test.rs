use base64::Engine;
use crypto::{ensure_keypair_ephemeral, ensure_pqc_init, sign_envelope};
use machine::podmesh_p2p::envelope::verify_flatbuffer_envelope;
use protocol::machine::{build_envelope_canonical, build_envelope_signed};
use std::time::Duration;

#[test]
fn test_flatbuffer_envelope_roundtrip() {
    // Test that FlatBuffer envelopes signed with canonical bytes verify correctly
    ensure_pqc_init().expect("PQC initialization failed");
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let payload = b"test payload data";
    let payload_type = "test";
    let nonce = format!(
        "test-nonce-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let timestamp = 1234567890u64;
    let alg = "ml-dsa-65";

    let canonical_bytes =
        build_envelope_canonical(payload, payload_type, &nonce, timestamp, alg, None);

    let (sig_b64, pub_b64) =
        sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign envelope");

    let signed_envelope = build_envelope_signed(
        payload,
        payload_type,
        &nonce,
        timestamp,
        alg,
        "ml-dsa-65",
        &sig_b64,
        &pub_b64,
        None,
    );

    let result = verify_flatbuffer_envelope(&signed_envelope, Duration::from_secs(300));

    assert!(
        result.is_ok(),
        "FlatBuffer envelope verification failed: {:?}",
        result.err()
    );

    let parts = result.unwrap();
    assert_eq!(parts.payload, payload, "Payload should match original");
}

#[test]
fn test_flatbuffer_envelope_invalid_signature() {
    ensure_pqc_init().expect("PQC initialization failed");
    let (pubb, _privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let payload = b"test payload data";
    let payload_type = "test";
    let nonce = "invalid-sig-test";
    let timestamp = 1234567890u64;
    let alg = "ml-dsa-65";

    let invalid_envelope = build_envelope_signed(
        payload,
        payload_type,
        nonce,
        timestamp,
        alg,
        "ml-dsa-65",
        "aW52YWxpZC1zaWduYXR1cmU=",
        &base64::engine::general_purpose::STANDARD.encode(pubb),
        None,
    );

    let result = verify_flatbuffer_envelope(&invalid_envelope, Duration::from_secs(300));
    assert!(
        result.is_err(),
        "Invalid signature should fail verification"
    );
}

#[test]
fn test_flatbuffer_envelope_replay_protection() {
    ensure_pqc_init().expect("PQC initialization failed");
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let payload = b"replay test payload";
    let payload_type = "test";
    let nonce = format!(
        "replay-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let timestamp = 1234567890u64;
    let alg = "ml-dsa-65";

    let canonical_bytes =
        build_envelope_canonical(payload, payload_type, &nonce, timestamp, alg, None);
    let (sig_b64, pub_b64) =
        sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign envelope");

    let envelope = build_envelope_signed(
        payload,
        payload_type,
        &nonce,
        timestamp,
        alg,
        "ml-dsa-65",
        &sig_b64,
        &pub_b64,
        None,
    );

    let first_result = verify_flatbuffer_envelope(&envelope, Duration::from_secs(300));
    assert!(
        first_result.is_ok(),
        "First verification should succeed: {:?}",
        first_result.err()
    );

    let second_result = verify_flatbuffer_envelope(&envelope, Duration::from_secs(300));
    assert!(
        second_result.is_err(),
        "Replay should be detected and rejected"
    );
}

#[test]
fn test_flatbuffer_envelope_signature_prefix_handling() {
    ensure_pqc_init().expect("PQC initialization failed");
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let payload = b"prefix test payload";
    let payload_type = "test";
    let nonce = "prefix-test-nonce";
    let timestamp = 1234567890u64;
    let alg = "ml-dsa-65";

    let canonical_bytes =
        build_envelope_canonical(payload, payload_type, nonce, timestamp, alg, None);
    let (sig_b64, pub_b64) =
        sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign envelope");

    let envelope_with_prefix = build_envelope_signed(
        payload,
        payload_type,
        nonce,
        timestamp,
        alg,
        "ml-dsa-65",
        &sig_b64,
        &pub_b64,
        None,
    );

    let result = verify_flatbuffer_envelope(&envelope_with_prefix, Duration::from_secs(300));
    assert!(
        result.is_ok(),
        "Envelope with explicit prefix should verify: {:?}",
        result.err()
    );

    let env =
        protocol::machine::root_as_envelope(&envelope_with_prefix).expect("should parse envelope");
    let sig_field = env.sig().unwrap_or("");
    assert!(
        sig_field.contains("ml-dsa-65:"),
        "Signature field should contain prefix: {}",
        sig_field
    );
}

#[test]
fn test_flatbuffer_envelope_empty_payload() {
    ensure_pqc_init().expect("PQC initialization failed");
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let payload = b"";
    let payload_type = "empty";
    let nonce = "empty-payload-test";
    let timestamp = 1234567890u64;
    let alg = "ml-dsa-65";

    let canonical_bytes =
        build_envelope_canonical(payload, payload_type, nonce, timestamp, alg, None);
    let (sig_b64, pub_b64) =
        sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign envelope");

    let envelope = build_envelope_signed(
        payload,
        payload_type,
        nonce,
        timestamp,
        alg,
        "ml-dsa-65",
        &sig_b64,
        &pub_b64,
        None,
    );

    let result = verify_flatbuffer_envelope(&envelope, Duration::from_secs(300));
    assert!(
        result.is_ok(),
        "Empty payload should verify correctly: {:?}",
        result.err()
    );

    let parts = result.unwrap();
    assert_eq!(parts.payload, payload, "Empty payload should be preserved");
}

#[test]
fn test_flatbuffer_envelope_large_payload() {
    ensure_pqc_init().expect("PQC initialization failed");
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let large_payload = vec![0x42u8; 1024 * 1024];
    let payload_type = "large";
    let nonce = "large-payload-test";
    let timestamp = 1234567890u64;
    let alg = "ml-dsa-65";

    let canonical_bytes =
        build_envelope_canonical(&large_payload, payload_type, nonce, timestamp, alg, None);
    let (sig_b64, pub_b64) =
        sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign envelope");

    let envelope = build_envelope_signed(
        &large_payload,
        payload_type,
        nonce,
        timestamp,
        alg,
        "ml-dsa-65",
        &sig_b64,
        &pub_b64,
        None,
    );

    let result = verify_flatbuffer_envelope(&envelope, Duration::from_secs(300));
    assert!(
        result.is_ok(),
        "Large payload should verify correctly: {:?}",
        result.err()
    );

    let parts = result.unwrap();
    assert_eq!(
        parts.payload, large_payload,
        "Large payload should be preserved"
    );
}

#[test]
fn test_flatbuffer_envelope_malformed_data() {
    let malformed_data = b"this is not a valid flatbuffer";

    let result = verify_flatbuffer_envelope(malformed_data, Duration::from_secs(300));
    assert!(result.is_err(), "Malformed data should fail to parse");
}
