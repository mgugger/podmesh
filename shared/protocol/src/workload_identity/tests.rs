use super::*;
use biscuit_auth::builder::BlockBuilder;

const NOW: u64 = 1_800_000_000;

fn fixture(expiry: Option<u64>) -> (EnrollmentDelegation, Vec<u8>, Vec<u8>) {
    let (owner, private) = crypto::generate_signing_keypair();
    let (issuer, issuer_private) = crypto::generate_signing_keypair();
    let scope = EnrollmentScope {
        tenant_owner: crypto::b64_encode(&owner),
        manifest_id: crate::route_id(&owner, "demo"),
        workload_id: crate::workload_id(&owner, "demo", 0),
        revision_id: crate::revision_id(b"test revision"),
        issuer_public_key_b64: crypto::b64_encode(&issuer),
        token_id: "generation-one".into(),
        issued_at_secs: NOW,
        expires_at_secs: expiry,
        max_child_lifetime_secs: MAX_IDENTITY_LIFETIME_SECS,
    };
    (
        EnrollmentDelegation::mint(&private, scope, NOW).unwrap(),
        private,
        issuer_private,
    )
}

fn leaf(parent: EnrollmentDelegation, private: &[u8], now: u64) -> WorkloadIdentity {
    WorkloadIdentity::mint(
        private,
        parent,
        "a".repeat(64),
        crypto::b64_encode(&[7; 32]),
        now,
        600,
    )
    .unwrap()
}

#[test]
fn standing_parent_supports_offline_child_renewal_but_children_expire() {
    let (parent, _, private) = fixture(None);
    let first = leaf(parent.clone(), &private, NOW);
    first.verify(Some(NOW)).unwrap();
    assert!(first.verify(Some(NOW + 601)).is_err());
    let renewed = leaf(parent.clone(), &private, NOW + 100_000);
    renewed.verify(Some(NOW + 100_000)).unwrap();
    let scope = &parent.scope;
    renewed
        .verify_session(
            &scope.tenant_owner,
            &scope.manifest_id,
            &"a".repeat(64),
            &[7; 32],
            NOW + 100_000,
        )
        .unwrap();
    assert!(
        renewed
            .verify_session(
                &scope.tenant_owner,
                &scope.manifest_id,
                &"b".repeat(64),
                &[7; 32],
                NOW + 100_000
            )
            .is_err()
    );
    assert!(
        renewed
            .verify_session(
                &scope.tenant_owner,
                &scope.manifest_id,
                &"a".repeat(64),
                &[8; 32],
                NOW + 100_000
            )
            .is_err()
    );
}

#[test]
fn finite_parent_caps_child_and_cannot_be_self_extended() {
    let (parent, _, private) = fixture(Some(NOW + 90));
    let child = leaf(parent.clone(), &private, NOW);
    assert_eq!(child.claims.expires_at_secs, NOW + 90);
    assert!(parent.verify(Some(NOW + 91)).is_err());
    assert!(
        WorkloadIdentity::mint(
            &private,
            parent,
            "a".repeat(64),
            crypto::b64_encode(&[7; 32]),
            NOW + 91,
            600
        )
        .is_err()
    );
    child.verify(None).unwrap(); // carriers can be reconciled without authority
    assert!(child.verify(Some(NOW + 91)).is_err());
}

#[test]
fn wrong_issuer_parent_subject_and_scope_are_refused() {
    let (parent, owner_private, private) = fixture(None);
    assert!(
        WorkloadIdentity::mint(
            &owner_private,
            parent.clone(),
            "a".repeat(64),
            crypto::b64_encode(&[7; 32]),
            NOW,
            600
        )
        .is_err()
    );
    let identity = leaf(parent.clone(), &private, NOW);
    let mut altered = identity.clone();
    altered.claims.endpoint_id = "b".repeat(64);
    assert!(altered.verify(Some(NOW)).is_err());
    let mut altered = identity.clone();
    altered.claims.signing_public_key_b64 = crypto::b64_encode(&[8; 32]);
    assert!(altered.verify(Some(NOW)).is_err());
    let mut altered = identity.clone();
    altered.delegation.scope.workload_id = "c".repeat(64);
    assert!(altered.verify(Some(NOW)).is_err());
    let (other, _, _) = fixture(None);
    let mut altered = identity;
    altered.delegation = other;
    assert!(altered.verify(Some(NOW)).is_err());
    assert!(
        parent
            .verify_scope(
                "foreign",
                &parent.scope.manifest_id,
                &parent.scope.workload_id,
                None,
                Some(NOW)
            )
            .is_err()
    );
}

#[test]
fn attenuation_narrows_but_cannot_rebind_authority_facts() {
    let (parent, _, private) = fixture(None);
    let original = leaf(parent, &private, NOW);
    let token = Biscuit::from(
        crypto::b64_decode(&original.biscuit_b64).unwrap(),
        biscuit_root_from_owner(
            &public_key(&original.delegation.scope.issuer_public_key_b64).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let restricted = token
        .append(
            BlockBuilder::new()
                .code("check if time($now), $now < 1800000010")
                .unwrap(),
        )
        .unwrap();
    let mut narrowed = original.clone();
    narrowed.biscuit_b64 = crypto::b64_encode(&restricted.to_vec().unwrap());
    narrowed.verify(Some(NOW)).unwrap();
    assert!(narrowed.verify(Some(NOW + 20)).is_err());
    let substituted = token
        .append(
            BlockBuilder::new()
                .fact(fact(
                    "claim",
                    &[string("endpoint"), string(&"b".repeat(64))],
                ))
                .unwrap(),
        )
        .unwrap();
    let mut widened = original;
    widened.biscuit_b64 = crypto::b64_encode(&substituted.to_vec().unwrap());
    widened.claims.endpoint_id = "b".repeat(64);
    assert!(widened.verify(Some(NOW)).is_err());
}

#[test]
fn strict_bounded_encodings_and_no_bearer_fallback() {
    let (parent, _, private) = fixture(None);
    let child = leaf(parent.clone(), &private, NOW);
    assert_eq!(
        EnrollmentDelegation::from_b64(&parent.to_b64().unwrap()).unwrap(),
        parent
    );
    assert_eq!(
        WorkloadIdentity::from_b64(&child.to_b64().unwrap()).unwrap(),
        child
    );
    assert!(WorkloadIdentity::from_bytes(&vec![b' '; MAX_WORKLOAD_IDENTITY_BYTES + 1]).is_err());
    assert!(WorkloadIdentity::from_b64(&parent.biscuit_b64).is_err());
    let mut value: serde_json::Value = serde_json::from_slice(&child.to_bytes().unwrap()).unwrap();
    value["extra"] = serde_json::json!("privilege");
    assert!(WorkloadIdentity::from_bytes(&serde_json::to_vec(&value).unwrap()).is_err());
}
