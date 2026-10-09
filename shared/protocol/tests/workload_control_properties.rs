use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use proptest::{prelude::*, test_runner::RngSeed};
use protocol::{
    AcceptedWorkloadPayload, PeerReplayRegistry, ReplayLimits, WorkloadEnvelopeParts,
    WorkloadPayload, WorkloadPayloadType, accept_workload_payload, seal_workload_payload,
};
use serde::{Deserialize, Serialize};

const U02_PROPTEST_SEED: u64 = 0x504f_444d_4553_4802;
const NOW: u64 = 1_700_000_000_000;
const NONCE: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

fn config() -> ProptestConfig {
    eprintln!("U-02 proptest fixed seed: {U02_PROPTEST_SEED}");
    ProptestConfig {
        cases: 256,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(U02_PROPTEST_SEED),
        ..ProptestConfig::default()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct GeneratedPayload {
    value: Vec<u8>,
}

impl WorkloadPayload for GeneratedPayload {
    const TYPE: WorkloadPayloadType = WorkloadPayloadType::RegistrationRequest;

    fn validate(&self, _now_secs: u64) -> anyhow::Result<()> {
        anyhow::ensure!(self.value.len() <= 4096, "generated payload too large");
        Ok(())
    }
}

fn endpoint(seed: u8) -> String {
    format!("{seed:02x}").repeat(32)
}

fn seal(payload: &GeneratedPayload, sender_seed: u8, key_seed: u8, nonce: &str) -> Vec<u8> {
    let private = [key_seed; crypto::ED25519_PRIVATE_KEY_SIZE];
    let public = ed25519_dalek::SigningKey::from_bytes(&private)
        .verifying_key()
        .to_bytes();
    seal_workload_payload(
        WorkloadEnvelopeParts {
            nonce,
            now_millis: NOW,
            sender_id: &endpoint(sender_seed),
            recipient_id: &endpoint(250),
            sender_signing_public: &public,
            sender_signing_private: &private,
            sender_kem_public: None,
        },
        payload,
    )
    .unwrap()
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn tp01_payload_serialization_round_trip(value in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let payload = GeneratedPayload { value };
        let bytes = postcard::to_allocvec(&payload).unwrap();
        prop_assert_eq!(postcard::from_bytes::<GeneratedPayload>(&bytes).unwrap(), payload);
    }

    #[test]
    fn tp02_typed_envelope_round_trip(value in proptest::collection::vec(any::<u8>(), 0..4096), sender in 1u8..200, key in any::<u8>()) {
        let payload = GeneratedPayload { value };
        let bytes = seal(&payload, sender, key, NONCE);
        let replay = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        let accepted: AcceptedWorkloadPayload<GeneratedPayload> = accept_workload_payload(
            &bytes, &endpoint(250), &endpoint(sender), NOW, &replay, Instant::now(),
        ).unwrap();
        prop_assert_eq!(accepted.payload, payload);
    }

    #[test]
    fn tp04_payload_tampering_is_refused(value in proptest::collection::vec(any::<u8>(), 1..256), sender in 1u8..200) {
        let payload = GeneratedPayload { value };
        let bytes = seal(&payload, sender, 7, NONCE);
        let mut envelope: protocol::machine::Envelope = postcard::from_bytes(&bytes).unwrap();
        envelope.payload[0] ^= 1;
        let tampered = postcard::to_allocvec(&envelope).unwrap();
        let replay = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        prop_assert!(accept_workload_payload::<GeneratedPayload>(
            &tampered, &endpoint(250), &endpoint(sender), NOW, &replay, Instant::now(),
        ).is_err());
    }

    #[test]
    fn tp05_wrong_recipient_is_refused(value in proptest::collection::vec(any::<u8>(), 0..256), sender in 1u8..200) {
        let bytes = seal(&GeneratedPayload { value }, sender, 8, NONCE);
        let replay = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        prop_assert!(accept_workload_payload::<GeneratedPayload>(
            &bytes, &endpoint(249), &endpoint(sender), NOW, &replay, Instant::now(),
        ).is_err());
    }

    #[test]
    fn tp06_oversized_payload_is_refused(size in 4097usize..8192) {
        let payload = GeneratedPayload { value: vec![0; size] };
        let private = [9u8; 32];
        let public = ed25519_dalek::SigningKey::from_bytes(&private).verifying_key().to_bytes();
        let result = seal_workload_payload(
            WorkloadEnvelopeParts {
                nonce: NONCE, now_millis: NOW, sender_id: &endpoint(1), recipient_id: &endpoint(250),
                sender_signing_public: &public, sender_signing_private: &private, sender_kem_public: None,
            }, &payload,
        );
        prop_assert!(result.is_err());
    }

    #[test]
    fn tp07_replay_is_non_idempotent(value in proptest::collection::vec(any::<u8>(), 0..256)) {
        let bytes = seal(&GeneratedPayload { value }, 1, 10, NONCE);
        let replay = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        prop_assert!(accept_workload_payload::<GeneratedPayload>(&bytes, &endpoint(250), &endpoint(1), NOW, &replay, Instant::now()).is_ok());
        prop_assert!(accept_workload_payload::<GeneratedPayload>(&bytes, &endpoint(250), &endpoint(1), NOW, &replay, Instant::now()).is_err());
    }

    #[test]
    fn tp09_replay_sequence_matches_set_model(nonces in proptest::collection::vec("[a-z0-9]{36,40}", 0..64)) {
        let replay = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        let mut model = HashSet::new();
        let now = Instant::now();
        for nonce in nonces {
            let expected = model.insert(nonce.clone());
            prop_assert_eq!(replay.check_and_insert("peer", &nonce, now).is_ok(), expected);
        }
    }

    #[test]
    fn tp16_distinct_peer_insertions_commute(left in "[a-z]{36,40}", right in "[a-z]{36,40}") {
        let first = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        let second = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        let now = Instant::now();
        first.check_and_insert("a", &left, now).unwrap();
        first.check_and_insert("b", &right, now).unwrap();
        second.check_and_insert("b", &right, now).unwrap();
        second.check_and_insert("a", &left, now).unwrap();
        prop_assert_eq!(first.check_and_insert("a", &left, now).is_err(), second.check_and_insert("a", &left, now).is_err());
        prop_assert_eq!(first.check_and_insert("b", &right, now).is_err(), second.check_and_insert("b", &right, now).is_err());
    }

    #[test]
    fn tp20_unknown_version_is_refused(value in proptest::collection::vec(any::<u8>(), 0..256)) {
        let bytes = seal(&GeneratedPayload { value }, 1, 11, NONCE);
        let mut envelope: protocol::machine::Envelope = postcard::from_bytes(&bytes).unwrap();
        envelope.version = envelope.version.saturating_add(1);
        let invalid = postcard::to_allocvec(&envelope).unwrap();
        let replay = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        prop_assert!(accept_workload_payload::<GeneratedPayload>(&invalid, &endpoint(250), &endpoint(1), NOW, &replay, Instant::now()).is_err());
    }

    #[test]
    fn tp21_non_announcement_key_rotation_is_accepted(value in proptest::collection::vec(any::<u8>(), 0..256), first_key in any::<u8>(), second_key in any::<u8>()) {
        let replay = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        let first = seal(&GeneratedPayload { value: value.clone() }, 1, first_key, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        let second = seal(&GeneratedPayload { value }, 1, second_key, "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
        prop_assert!(accept_workload_payload::<GeneratedPayload>(&first, &endpoint(250), &endpoint(1), NOW, &replay, Instant::now()).is_ok());
        prop_assert!(accept_workload_payload::<GeneratedPayload>(&second, &endpoint(250), &endpoint(1), NOW, &replay, Instant::now() + Duration::from_millis(1)).is_ok());
    }
}

#[test]
fn tp03_registry_is_total_unique_and_paired() {
    let names = WorkloadPayloadType::ALL.map(WorkloadPayloadType::wire_name);
    assert_eq!(names.into_iter().collect::<HashSet<_>>().len(), 12);
    assert_eq!(
        WorkloadPayloadType::ALL
            .into_iter()
            .filter(|item| item.response().is_some())
            .count(),
        6
    );
}

#[test]
fn tp08_equal_nonce_namespaces_are_per_peer() {
    let replay = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
    let now = Instant::now();
    replay.check_and_insert("a", NONCE, now).unwrap();
    replay.check_and_insert("b", NONCE, now).unwrap();
}

#[test]
fn tp10_capacity_evicts_and_continues() {
    let limits = ReplayLimits {
        max_peers: 1,
        nonces_per_peer: 1,
        max_nonce_bytes: 128,
        retention: Duration::from_secs(60),
    };
    let replay = PeerReplayRegistry::new(limits).unwrap();
    let now = Instant::now();
    replay.check_and_insert("a", NONCE, now).unwrap();
    replay.check_and_insert("b", NONCE, now).unwrap();
    replay.check_and_insert("a", NONCE, now).unwrap();
}
