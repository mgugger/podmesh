use crypto::{b64_decode, b64_encode, ensure_keypair_ephemeral, sign_envelope};
use protocol::machine::{build_envelope_canonical, build_envelope_signed};
use std::time::Duration;

/// Get current timestamp in milliseconds for test envelopes
fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn test_apply_request_envelope() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let apply_request = protocol::machine::build_apply_request(
        3,
        "apply-op-123",
        "apiVersion: v1\nkind: Pod",
        "origin-peer-id",
        "test-manifest-id",
    );

    let payload_type = "apply_request";
    let nonce = format!(
        "apply-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let alg = "ed25519";

    let canonical_bytes =
        build_envelope_canonical(&apply_request, payload_type, &nonce, timestamp, alg, None);

    let (sig_b64, pub_b64) =
        sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign apply envelope");

    let apply_envelope = build_envelope_signed(
        &apply_request,
        payload_type,
        &nonce,
        timestamp,
        alg,
        "ed25519",
        &sig_b64,
        &pub_b64,
        None,
    );

    let verification_result = podmesh_scheduler::podmesh_p2p::envelope::verify_envelope(
        &apply_envelope,
        Duration::from_secs(300),
    );

    assert!(
        verification_result.is_ok(),
        "Apply envelope should verify correctly: {:?}",
        verification_result.err()
    );

    let parts = verification_result.unwrap();
    let parsed_apply = protocol::machine::root_as_apply_request(&parts.payload)
        .expect("Should parse apply request from envelope payload");

    assert_eq!(parsed_apply.replicas(), 3);
    assert_eq!(parsed_apply.operation_id().unwrap_or(""), "apply-op-123");
    assert!(
        parsed_apply
            .manifest_json()
            .unwrap_or("")
            .contains("kind: Pod")
    );
}

#[test]
fn test_envelope_base64_encoding_for_transport() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let payload = b"test transport payload";
    let payload_type = "transport_test";
    let nonce = format!("transport-nonce-{}", current_timestamp_ms());
    let timestamp = current_timestamp_ms();
    let alg = "ed25519";

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
        "ed25519",
        &sig_b64,
        &pub_b64,
        None,
    );

    let encoded_envelope = b64_encode(&envelope);
    assert!(!encoded_envelope.is_empty());

    let decoded_envelope = b64_decode(&encoded_envelope)
        .expect("Should decode base64 envelope");

    let verification_result = podmesh_scheduler::podmesh_p2p::envelope::verify_envelope(
        &decoded_envelope,
        Duration::from_secs(300),
    );

    assert!(
        verification_result.is_ok(),
        "Decoded envelope should verify correctly: {:?}",
        verification_result.err()
    );

    let parts = verification_result.unwrap();
    assert_eq!(parts.payload, payload);
}
