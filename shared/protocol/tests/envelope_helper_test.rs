use crypto::{ensure_keypair_ephemeral, sign_envelope};
use protocol::machine::build_envelope_signed;

#[test]
fn envelope_helper_roundtrip() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("keypair");

    let payload = b"hello-postcard".to_vec();
    let payload_type = "test";
    let nonce = "n-123";
    let ts = 42u64;
    let alg = "ed25519";

    // Build canonical bytes and sign using crypto helper
    let canonical =
        protocol::machine::build_envelope_canonical(&payload, payload_type, nonce, ts, alg, None);
    let (sig_b64, pub_b64) = sign_envelope(&privb, &pubb, &canonical).expect("sign");

    // Build a signed postcard envelope using the same format
    let envelope = build_envelope_signed(
        &payload,
        payload_type,
        nonce,
        ts,
        alg,
        "ed25519",
        &sig_b64,
        &pub_b64,
        None,
    );

    // Use helper to extract and verify fields
    let (canon2, sig_bytes, pub_bytes, sig_field, pub_field) =
        protocol::machine::envelope_extract_sig_pub_legacy(&envelope).expect("extract");

    // canonical bytes should match
    assert_eq!(canonical, canon2);
    // signature/pub decode to non-empty bytes
    assert!(!sig_bytes.is_empty());
    assert!(!pub_bytes.is_empty());
    assert!(sig_field.contains("ed25519"));
    assert_eq!(pub_field, pub_b64);
}
