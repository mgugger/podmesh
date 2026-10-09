use std::time::Instant;

use podmesh_proxy::tenant_sessions::{ProvenTenant, TenantSessions};
use proptest::{prelude::*, test_runner::RngSeed};
use protocol::{
    IngressResponseMetadata, PeerReplayRegistry, ReplayLimits, WorkloadEnvelopeParts,
    WorkloadPayload, WorkloadPayloadType, WorkloadStreamPhase, accept_workload_payload,
    seal_workload_payload,
};
use serde::{Deserialize, Serialize};

const U02_PROPTEST_SEED: u64 = 0x504f_444d_4553_4802;
const NOW: u64 = 1_700_000_000_000;
const STABLE_ID: usize = 7;

fn config() -> ProptestConfig {
    eprintln!("U-02 integration proptest fixed seed: {U02_PROPTEST_SEED}");
    ProptestConfig {
        cases: 256,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(U02_PROPTEST_SEED),
        ..ProptestConfig::default()
    }
}

fn endpoint(seed: u8) -> iroh::EndpointId {
    iroh::SecretKey::from_bytes(&[seed; 32]).public()
}

fn tenant(label: u8) -> ProvenTenant {
    ProvenTenant {
        owner_pubkey: format!("owner-{label}"),
        manifest_id: format!("manifest-{label}"),
        credential: vec![label],
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ModelPayload(u8);

impl WorkloadPayload for ModelPayload {
    const TYPE: WorkloadPayloadType = WorkloadPayloadType::RegistrationRequest;

    fn validate(&self, _now_secs: u64) -> anyhow::Result<()> {
        Ok(())
    }
}

fn sealed_model_payload(nonce: &str) -> Vec<u8> {
    let private = [9u8; 32];
    let public = ed25519_dalek::SigningKey::from_bytes(&private)
        .verifying_key()
        .to_bytes();
    seal_workload_payload(
        WorkloadEnvelopeParts {
            nonce,
            now_millis: NOW,
            sender_id: &endpoint(1).to_string(),
            recipient_id: &endpoint(2).to_string(),
            sender_signing_public: &public,
            sender_signing_private: &private,
            sender_kem_public: None,
        },
        &ModelPayload(1),
    )
    .unwrap()
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn tp11_session_trace_matches_one_handshake_model(commands in proptest::collection::vec(any::<bool>(), 0..64), label in any::<u8>()) {
        let sessions = TenantSessions::new();
        let peer = endpoint(1);
        sessions.begin(peer, STABLE_ID).unwrap();
        let mut model_proven = false;
        for prove in commands {
            if prove {
                let actual = sessions.prove_once(peer, STABLE_ID, tenant(label)).is_ok();
                prop_assert_eq!(actual, !model_proven);
                model_proven = true;
            } else {
                prop_assert_eq!(sessions.proven(&peer, STABLE_ID).is_some(), model_proven);
            }
        }
    }

    #[test]
    fn tp12_envelope_does_not_authorize_a_different_workload(label in "[a-z]{1,32}") {
        let (public, private) = crypto::generate_signing_keypair();
        let owner = crypto::b64_encode(&public);
        let manifest = protocol::route_id(&public, &label);
        let claims = protocol::WorkloadCredentialClaims {
            tenant_owner: owner.clone(), manifest_id: manifest.clone(), issued_at_secs: NOW / 1000,
            expires_at_secs: NOW / 1000 + 60, token_id: "property".into(),
        };
        let credential = protocol::mint_workload_credential(&private, &public, &claims, NOW / 1000).unwrap();
        prop_assert!(protocol::verify_workload_credential(&credential, &owner, &manifest, NOW / 1000).is_ok());
        prop_assert!(protocol::verify_workload_credential(&credential, &owner, "other", NOW / 1000).is_err());
    }

    #[test]
    fn tp13_raw_mode_requires_setup_and_authority(setup in any::<bool>(), authority in any::<bool>()) {
        let mut phase = WorkloadStreamPhase::default();
        if setup { phase.accept_control().unwrap(); }
        if authority { let _ = phase.authorize(); }
        prop_assert_eq!(phase.enter_raw().is_ok(), setup && authority);
    }

    #[test]
    fn tp14_websocket_transition_requires_requested_signed_101(requested in any::<bool>(), status in 100u16..600, accepted in any::<bool>()) {
        let metadata = IngressResponseMetadata { status_code: status, headers: Vec::new(), upgrade_accepted: accepted };
        let valid = metadata.validate().is_ok();
        let mut phase = WorkloadStreamPhase::default();
        phase.accept_control().unwrap();
        phase.authorize().unwrap();
        let duplex = phase.enter_websocket(requested, status, accepted).is_ok();
        prop_assert_eq!(duplex, requested && accepted && status == 101 && valid);
        if accepted && status != 101 { prop_assert!(!valid); }
    }

    #[test]
    fn tp15_announcement_validation_does_not_create_tenant(label in any::<u8>()) {
        let sessions = TenantSessions::new();
        let peer = endpoint(label);
        sessions.begin(peer, STABLE_ID).unwrap();
        let private = [label; 32];
        let public = ed25519_dalek::SigningKey::from_bytes(&private).verifying_key().to_bytes();
        let record = protocol::EndpointRecord {
            version: protocol::ENDPOINT_RECORD_VERSION,
            endpoint_id: peer.as_bytes().to_vec(),
            relay_url: None,
            direct_addresses: vec!["127.0.0.1:4000".into()],
            signing_pubkey: String::new(),
            issued_at_secs: NOW / 1000,
            expires_at_secs: NOW / 1000 + 60,
            signature: String::new(),
        }.sign(&public, &private, NOW / 1000).unwrap();
        let request = protocol::ProxyAnnouncementRequest { endpoint: record };
        let bytes = seal_workload_payload(
            WorkloadEnvelopeParts {
                nonce: "dddddddd-dddd-dddd-dddd-dddddddddddd", now_millis: NOW,
                sender_id: &peer.to_string(), recipient_id: &endpoint(250).to_string(),
                sender_signing_public: &public, sender_signing_private: &private, sender_kem_public: None,
            }, &request,
        ).unwrap();
        let replay = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        prop_assert!(accept_workload_payload::<protocol::ProxyAnnouncementRequest>(
            &bytes, &endpoint(250).to_string(), &peer.to_string(), NOW, &replay, Instant::now(),
        ).is_ok());
        prop_assert!(sessions.proven(&peer, STABLE_ID).is_none());
    }

    #[test]
    fn tp17_distinct_session_operations_commute(left in any::<u8>(), right in any::<u8>()) {
        prop_assume!(left != right);
        let first = TenantSessions::new();
        let second = TenantSessions::new();
        let a = endpoint(left);
        let b = endpoint(right);
        first.begin(a, 1).unwrap(); first.begin(b, 2).unwrap();
        second.begin(b, 2).unwrap(); second.begin(a, 1).unwrap();
        prop_assert_eq!(first.proven(&a, 1), second.proven(&a, 1));
        prop_assert_eq!(first.proven(&b, 2), second.proven(&b, 2));
    }

    #[test]
    fn tp18_every_raw_trace_prefix_has_prior_guards(actions in proptest::collection::vec(0u8..3, 0..64)) {
        let mut phase = WorkloadStreamPhase::default();
        for action in actions {
            match action {
                0 => { let _ = phase.accept_control(); }
                1 => { let _ = phase.authorize(); }
                2 => { let _ = phase.enter_raw(); }
                _ => unreachable!(),
            }
            if phase == WorkloadStreamPhase::Raw {
                prop_assert!(phase.accept_control().is_err());
                prop_assert!(phase.authorize().is_err());
            }
        }
    }

    #[test]
    fn tp19_failed_envelope_does_not_burn_nonce(nonce in "[a-z0-9]{36,40}") {
        let bytes = sealed_model_payload(&nonce);
        let mut envelope: protocol::machine::Envelope = postcard::from_bytes(&bytes).unwrap();
        envelope.payload.push(9);
        let tampered = postcard::to_allocvec(&envelope).unwrap();
        let replay = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        prop_assert!(accept_workload_payload::<ModelPayload>(&tampered, &endpoint(2).to_string(), &endpoint(1).to_string(), NOW, &replay, Instant::now()).is_err());
        prop_assert!(accept_workload_payload::<ModelPayload>(&bytes, &endpoint(2).to_string(), &endpoint(1).to_string(), NOW, &replay, Instant::now()).is_ok());
    }
}
