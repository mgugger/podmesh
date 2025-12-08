use crypto::{b64_encode, ensure_keypair_ephemeral, sign_envelope};
use podmesh_scheduler::podmesh_p2p::envelope::verify_envelope;
use protocol::machine::{build_envelope_canonical, build_envelope_signed};
use std::time::Duration;

#[test]
fn test_signature_verification_error_messages() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");
    let (pubb2, _privb2) = ensure_keypair_ephemeral().expect("Failed to generate second keypair");

    let payload = b"error handling test payload";
    let canonical_bytes = build_envelope_canonical(
        payload,
        "test",
        "error-test-nonce",
        1234567890,
        "ed25519",
        None,
    );

    {
        let invalid_sig = "not-valid-base64!@#$%";
        let pub_b64 = b64_encode(&pubb);

        let invalid_sig_envelope = build_envelope_signed(
            payload,
            "test",
            "invalid-sig-nonce",
            1234567890,
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

    {
        let invalid_sig = b64_encode(b"invalid signature bytes that are too short");
        let pub_b64 = b64_encode(&pubb);

        let invalid_length_envelope = build_envelope_signed(
            payload,
            "test",
            "invalid-length-nonce",
            1234567890,
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

    {
        let (sig_b64, _pub_b64) = sign_envelope(&privb, &pubb, &canonical_bytes)
            .expect("Failed to sign with correct keypair");
        let wrong_pub_b64 = b64_encode(&pubb2);

        let wrong_key_envelope = build_envelope_signed(
            payload,
            "test",
            "wrong-key-nonce",
            1234567890,
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

    {
        let original_payload = b"original payload for signing";
        let tampered_payload = b"tampered payload content";

        let original_canonical = build_envelope_canonical(
            original_payload,
            "test",
            "tamper-nonce",
            1234567890,
            "ed25519",
            None,
        );

        let (sig_b64, pub_b64) = sign_envelope(&privb, &pubb, &original_canonical)
            .expect("Failed to sign original payload");

        let tampered_envelope = build_envelope_signed(
            tampered_payload,
            "test",
            "tamper-nonce",
            1234567890,
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

    {
        let (sig_b64, _) =
            sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign envelope");
        let invalid_pub =
            b64_encode(b"invalid public key bytes");

        let invalid_pub_envelope = build_envelope_signed(
            payload,
            "test",
            "invalid-pub-nonce",
            1234567890,
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
}

#[test]
fn test_envelope_parsing_robustness() {

    {
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

    {
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

    {
        let empty_data = b"";
        let result = protocol::machine::root_as_envelope(empty_data);
        assert!(result.is_err(), "Empty data should not parse as envelope");

        let verify_result = verify_envelope(empty_data, Duration::from_secs(300));
        assert!(
            verify_result.is_err(),
            "Empty envelope should fail verification"
        );
    }

    {
        let envelope_with_empty_sig = build_envelope_signed(
            b"test payload",
            "test",
            "empty-sig-nonce",
            1234567890,
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

    {
        let malformed_b64_envelope = build_envelope_signed(
            b"test payload",
            "test",
            "malformed-b64-nonce",
            1234567890,
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
}

#[test]
fn test_nonce_validation_errors() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let payload = b"nonce validation test";

    {
        let canonical_bytes = build_envelope_canonical(
            payload,
            "test",
            "duplicate-nonce-123",
            1234567890,
            "ed25519",
            None,
        );

        let (sig_b64, pub_b64) =
            sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign envelope");

        let envelope = build_envelope_signed(
            payload,
            "test",
            "duplicate-nonce-123",
            1234567890,
            "ed25519",
            "ed25519",
            &sig_b64,
            &pub_b64,
            None,
        );

        let first_result = verify_envelope(&envelope, Duration::from_secs(300));
        assert!(first_result.is_ok(), "First verification should succeed");

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

    {
        let canonical_empty_nonce =
            build_envelope_canonical(payload, "test", "", 1234567890, "ed25519", None);

        let (sig_b64, pub_b64) = sign_envelope(&privb, &pubb, &canonical_empty_nonce)
            .expect("Failed to sign envelope with empty nonce");

        let envelope_empty_nonce = build_envelope_signed(
            payload,
            "test",
            "",
            1234567890,
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
}

#[test]
fn test_algorithm_mismatch_errors() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let payload = b"algorithm test payload";

    let canonical_bytes = build_envelope_canonical(
        payload,
        "test",
        "alg-test-nonce",
        1234567890,
        "different-algorithm",
        None,
    );

    let (sig_b64, pub_b64) =
        sign_envelope(&privb, &pubb, &canonical_bytes).expect("Failed to sign envelope");

    let envelope = build_envelope_signed(
        payload,
        "test",
        "alg-test-nonce",
        1234567890,
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

#[test]
fn test_timestamp_edge_cases() {
    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let payload = b"timestamp test payload";

    {
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
            result.is_ok(),
            "Zero timestamp should not prevent verification: {:?}",
            result.err()
        );
    }

    {
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
            result.is_ok(),
            "Maximum timestamp should not prevent verification: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_concurrent_nonce_validation() {
    use std::sync::Arc;
    use std::thread;

    let (pubb, privb) = ensure_keypair_ephemeral().expect("Failed to generate keypair");

    let payload = b"concurrent nonce test";
    let base_nonce = "concurrent-test";

    let mut handles = vec![];
    let success_count = Arc::new(std::sync::Mutex::new(0));

    for i in 0..10 {
        let nonce = format!("{}-{}", base_nonce, i);
        let pubb_clone = pubb.clone();
        let privb_clone = privb.clone();
        let payload_clone = payload.to_vec();
        let success_count_clone = Arc::clone(&success_count);

        let handle = thread::spawn(move || {
            let canonical = build_envelope_canonical(
                &payload_clone,
                "test",
                &nonce,
                1234567890 + i as u64,
                "ed25519",
                None,
            );

            let (sig_b64, pub_b64) = sign_envelope(&privb_clone, &pubb_clone, &canonical)
                .expect("Failed to sign in thread");

            let envelope = build_envelope_signed(
                &payload_clone,
                "test",
                &nonce,
                1234567890 + i as u64,
                "ed25519",
                "ed25519",
                &sig_b64,
                &pub_b64,
                None,
            );

            let result = verify_envelope(&envelope, Duration::from_secs(300));
            if result.is_ok() {
                let mut count = success_count_clone.lock().unwrap();
                *count += 1;
            }
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread should complete successfully");
    }

    let final_count = *success_count.lock().unwrap();
    assert_eq!(
        final_count, 10,
        "All concurrent envelope verifications should succeed"
    );
}
