use protocol::{
    IROH_ENDPOINT_ID_BYTES, MACHINE_RELAY_GRANT_VERSION, MAX_MACHINE_RELAY_AUTH_TOKEN_LEN,
    MAX_MACHINE_RELAY_GRANT_BYTES, MachineRelayGrant, MachineRole,
};

const NOW: u64 = 20_000;
const RELAY: &str = "https://relay.example.test";

fn signed_grant(role: MachineRole) -> (MachineRelayGrant, Vec<u8>) {
    let (issuer_public, issuer_private) = crypto::ensure_keypair_ephemeral().unwrap();
    let grant = MachineRelayGrant {
        version: MACHINE_RELAY_GRANT_VERSION,
        subject_endpoint_id: vec![7; IROH_ENDPOINT_ID_BYTES],
        role,
        relay_audience: RELAY.into(),
        issued_at_secs: NOW,
        expires_at_secs: NOW + 60,
        token_id: "grant-1".into(),
        issuer_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(&issuer_public, &issuer_private, NOW)
    .unwrap();
    (grant, issuer_public)
}

#[test]
fn machine_roles_verify_and_roundtrip() {
    for role in [
        MachineRole::Scheduler,
        MachineRole::Agent,
        MachineRole::Podctl,
    ] {
        let (grant, issuer) = signed_grant(role);
        let decoded = MachineRelayGrant::from_bytes(&grant.to_bytes(NOW).unwrap(), NOW).unwrap();
        decoded
            .verify(&[issuer], &[7; IROH_ENDPOINT_ID_BYTES], RELAY, NOW)
            .unwrap();
    }
}

#[test]
fn bearer_token_roundtrips_and_is_bounded_before_decode() {
    let (grant, _) = signed_grant(MachineRole::Agent);
    let token = grant.to_auth_token(NOW).unwrap();
    assert_eq!(
        MachineRelayGrant::from_auth_token(&token, NOW).unwrap(),
        grant
    );
    assert!(
        MachineRelayGrant::from_auth_token(&"A".repeat(MAX_MACHINE_RELAY_AUTH_TOKEN_LEN + 1), NOW)
            .is_err()
    );
}

#[test]
fn workload_roles_are_rejected() {
    for role in [MachineRole::Proxy, MachineRole::Sidecar] {
        let (grant, issuer) = signed_grant(role);
        assert!(
            grant
                .verify(&[issuer], &[7; IROH_ENDPOINT_ID_BYTES], RELAY, NOW)
                .is_err()
        );
    }
}

#[test]
fn rejects_wrong_subject_audience_and_issuer() {
    let (grant, issuer) = signed_grant(MachineRole::Agent);
    assert!(
        grant
            .verify(&[issuer.clone()], &[8; IROH_ENDPOINT_ID_BYTES], RELAY, NOW)
            .is_err()
    );
    assert!(
        grant
            .verify(
                &[issuer.clone()],
                &[7; IROH_ENDPOINT_ID_BYTES],
                "https://other-relay.example.test",
                NOW,
            )
            .is_err()
    );
    let (other_issuer, _) = crypto::ensure_keypair_ephemeral().unwrap();
    assert!(
        grant
            .verify(&[other_issuer], &[7; IROH_ENDPOINT_ID_BYTES], RELAY, NOW)
            .is_err()
    );
}

#[test]
fn signature_binds_role_and_audience() {
    let (mut grant, issuer) = signed_grant(MachineRole::Agent);
    grant.role = MachineRole::Scheduler;
    assert!(
        grant
            .verify(&[issuer.clone()], &[7; IROH_ENDPOINT_ID_BYTES], RELAY, NOW)
            .is_err()
    );

    let (mut grant, issuer) = signed_grant(MachineRole::Agent);
    grant.relay_audience = "https://changed-relay.example.test".into();
    assert!(
        grant
            .verify(
                &[issuer],
                &[7; IROH_ENDPOINT_ID_BYTES],
                "https://changed-relay.example.test",
                NOW,
            )
            .is_err()
    );
}

#[test]
fn rejects_expired_and_unbounded_grants() {
    let (grant, issuer) = signed_grant(MachineRole::Agent);
    assert!(
        grant
            .verify(&[issuer], &[7; IROH_ENDPOINT_ID_BYTES], RELAY, NOW + 61)
            .is_err()
    );

    let bytes = vec![0; MAX_MACHINE_RELAY_GRANT_BYTES + 1];
    assert!(MachineRelayGrant::from_bytes(&bytes, NOW).is_err());
}
