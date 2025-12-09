//! Concurrent envelope verification tests.
//!
//! Tests that envelope verification works correctly under concurrent load.

use crypto::{ensure_keypair_ephemeral, sign_envelope};
use podmesh_scheduler::podmesh_p2p::envelope::verify_envelope;
use protocol::machine::{build_envelope_canonical, build_envelope_signed};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn test_concurrent_nonce_validation() {
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
            let ts = current_timestamp_ms() + i as u64;
            let canonical =
                build_envelope_canonical(&payload_clone, "test", &nonce, ts, "ed25519", None);

            let (sig_b64, pub_b64) = sign_envelope(&privb_clone, &pubb_clone, &canonical)
                .expect("Failed to sign in thread");

            let envelope = build_envelope_signed(
                &payload_clone,
                "test",
                &nonce,
                ts,
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
