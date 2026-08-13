//! Tests for owner-signed workload credentials.

use super::super::workload_credential::*;

const NOW: u64 = 1_700_000_000;

fn claims(owner_public: &[u8], manifest_id: &str) -> WorkloadCredentialClaims {
    WorkloadCredentialClaims {
        tenant_owner: crypto::b64_encode(owner_public),
        manifest_id: manifest_id.to_string(),
        issued_at_secs: NOW,
        expires_at_secs: NOW + 3600,
        token_id: "credential-1".into(),
    }
}

#[test]
fn a_minted_credential_verifies_for_its_own_tenant_and_workload() {
    let (public, private) = crypto::generate_signing_keypair();
    let manifest = crate::route_id(&public, "web");
    let encoded =
        mint_workload_credential(&private, &public, &claims(&public, &manifest), NOW).unwrap();
    verify_workload_credential(&encoded, &crypto::b64_encode(&public), &manifest, NOW).unwrap();
}

/// The whole point: naming another tenant must be worthless without that
/// tenant's private key, or any caller could register that tenant's routes.
#[test]
fn a_credential_cannot_be_minted_for_a_tenant_whose_key_is_not_held() {
    let (victim_public, _) = crypto::generate_signing_keypair();
    let (attacker_public, attacker_private) = crypto::generate_signing_keypair();
    let manifest = crate::route_id(&victim_public, "web");
    // The attacker mints with its own key but names the victim as owner.
    let forged = mint_workload_credential(
        &attacker_private,
        &attacker_public,
        &claims(&attacker_public, &manifest),
        NOW,
    )
    .unwrap();
    assert!(
        verify_workload_credential(&forged, &crypto::b64_encode(&victim_public), &manifest, NOW)
            .is_err(),
        "a credential signed by another key must not prove the victim's tenancy"
    );
}

/// A credential is bound to one workload, so a tenant's own credential for
/// one deployment cannot be used to claim another deployment's routes.
#[test]
fn a_credential_does_not_carry_to_another_workload_of_the_same_tenant() {
    let (public, private) = crypto::generate_signing_keypair();
    let issued_for = crate::route_id(&public, "web");
    let other = crate::route_id(&public, "database");
    let encoded =
        mint_workload_credential(&private, &public, &claims(&public, &issued_for), NOW).unwrap();
    assert!(
        verify_workload_credential(&encoded, &crypto::b64_encode(&public), &other, NOW).is_err()
    );
}

#[test]
fn an_expired_credential_is_refused() {
    let (public, private) = crypto::generate_signing_keypair();
    let manifest = crate::route_id(&public, "web");
    let encoded =
        mint_workload_credential(&private, &public, &claims(&public, &manifest), NOW).unwrap();
    assert!(
        verify_workload_credential(
            &encoded,
            &crypto::b64_encode(&public),
            &manifest,
            NOW + 7200
        )
        .is_err()
    );
}

/// A proxy grant and a workload credential are both owner-signed Biscuits,
/// so the role and operation facts are what keep one from being replayed as
/// the other.
#[test]
fn a_proxy_grant_does_not_verify_as_a_workload_credential() {
    let (public, private) = crypto::generate_signing_keypair();
    let manifest = crate::route_id(&public, "web");
    let grant = crate::mint_proxy_grant(
        &private,
        &public,
        &crate::ProxyGrantClaims {
            tenant_owner: crypto::b64_encode(&public),
            proxy_endpoint: manifest.clone(),
            issued_at_secs: NOW,
            expires_at_secs: NOW + 3600,
            token_id: "grant-1".into(),
        },
        NOW,
    )
    .unwrap();
    assert!(
        verify_workload_credential(&grant, &crypto::b64_encode(&public), &manifest, NOW).is_err()
    );
}

#[test]
fn an_oversized_or_empty_credential_is_refused_before_decoding() {
    let owner = crypto::b64_encode(&crypto::generate_signing_keypair().0);
    assert!(verify_workload_credential(&[], &owner, "manifest", NOW).is_err());
    assert!(workload_credential_from_b64("").is_err());
    assert!(
        workload_credential_from_b64(&"a".repeat(MAX_WORKLOAD_CREDENTIAL_B64_LEN + 1)).is_err()
    );
}

/// A pod may outlive the credential it was deployed with, and an agent
/// re-reads its stored execution specification on every restart. Enforcing
/// expiry there would delete healthy long-running workloads on an unrelated
/// restart, so the structural check must ignore the validity window.
#[test]
fn an_expired_credential_still_verifies_structurally() {
    let (public, private) = crypto::generate_signing_keypair();
    let manifest = crate::route_id(&public, "web");
    let encoded =
        mint_workload_credential(&private, &public, &claims(&public, &manifest), NOW).unwrap();
    let long_after = NOW + MAX_WORKLOAD_CREDENTIAL_LIFETIME_SECS * 10;
    assert!(
        verify_workload_credential(
            &encoded,
            &crypto::b64_encode(&public),
            &manifest,
            long_after
        )
        .is_err(),
        "the proxy must still refuse an expired credential"
    );
    verify_workload_credential_structure(&encoded, &crypto::b64_encode(&public), &manifest)
        .expect("an agent must still be able to restore a long-running workload");
}

/// Ignoring expiry must not mean ignoring the signature.
#[test]
fn the_structural_check_still_refuses_a_forged_credential() {
    let (victim_public, _) = crypto::generate_signing_keypair();
    let (attacker_public, attacker_private) = crypto::generate_signing_keypair();
    let manifest = crate::route_id(&victim_public, "web");
    let forged = mint_workload_credential(
        &attacker_private,
        &attacker_public,
        &claims(&attacker_public, &manifest),
        NOW,
    )
    .unwrap();
    assert!(
        verify_workload_credential_structure(
            &forged,
            &crypto::b64_encode(&victim_public),
            &manifest
        )
        .is_err()
    );
}
