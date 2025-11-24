use anyhow::Context;
use base64::Engine as _;
use base64::engine::general_purpose;

use crypto::{ensure_keypair_ephemeral, ensure_pqc_init, sign_envelope, verify_envelope};
use protocol::machine::{build_envelope_canonical, build_envelope_signed, root_as_envelope};

#[test]
fn postcard_envelope_sign_and_verify() -> anyhow::Result<()> {
    ensure_pqc_init()?;
    let (pub_bytes, priv_bytes) = ensure_keypair_ephemeral()?;
    let payload_bytes = b"hello inner payload";

    let canonical_bytes = build_envelope_canonical(
        payload_bytes,
        "test.payload.v1",
        "nonce-abc-123",
        123_456_789,
        "ml-dsa-65",
        None,
    );

    let (sig_b64, pub_b64) = sign_envelope(&priv_bytes, &pub_bytes, &canonical_bytes)?;

    let signed_bytes = build_envelope_signed(
        payload_bytes,
        "test.payload.v1",
        "nonce-abc-123",
        123_456_789,
        "ml-dsa-65",
        "ml-dsa-65",
        &sig_b64,
        &pub_b64,
        None,
    );

    let sig_bytes = general_purpose::STANDARD
        .decode(&sig_b64)
        .context("decode sig b64")?;

    verify_envelope(&pub_bytes, &canonical_bytes, &sig_bytes)?;

    let mut tampered = canonical_bytes.clone();
    if !tampered.is_empty() {
        tampered[0] ^= 0xff;
    }
    assert!(
        verify_envelope(&pub_bytes, &tampered, &sig_bytes).is_err(),
        "tampered buffer should not verify",
    );

    let parsed = root_as_envelope(&signed_bytes)?;
    assert_eq!(parsed.payload(), Some(&payload_bytes[..]));
    assert_eq!(parsed.payload_type(), Some("test.payload.v1"));
    let expected_sig = format!("ml-dsa-65:{sig_b64}");
    assert_eq!(parsed.sig(), Some(expected_sig.as_str()));
    assert_eq!(parsed.pubkey(), Some(pub_b64.as_str()));

    Ok(())
}
