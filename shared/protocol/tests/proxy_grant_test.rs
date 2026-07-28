use protocol::{ProxyGrantClaims, mint_proxy_grant, verify_proxy_grant};

const NOW: u64 = 1_700_000_000;
const PROXY: &str = "3f2a9c1d4b5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";
const OTHER_PROXY: &str = "aa2a9c1d4b5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";

fn owner() -> (Vec<u8>, Vec<u8>) {
    let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
    (public, private)
}

fn claims(owner_public: &[u8], proxy_endpoint: &str, lifetime_secs: u64) -> ProxyGrantClaims {
    ProxyGrantClaims {
        tenant_owner: crypto::b64_encode(owner_public),
        proxy_endpoint: proxy_endpoint.to_string(),
        issued_at_secs: NOW,
        expires_at_secs: NOW + lifetime_secs,
        token_id: "token-1".to_string(),
    }
}

#[test]
fn a_sidecar_accepts_a_grant_its_owner_minted_for_that_proxy() {
    let (public, private) = owner();
    let grant = mint_proxy_grant(&private, &public, &claims(&public, PROXY, 3600), NOW).unwrap();
    verify_proxy_grant(&grant, &public, &crypto::b64_encode(&public), PROXY, NOW).unwrap();
}

#[test]
fn a_grant_cannot_be_replayed_by_a_different_proxy() {
    let (public, private) = owner();
    let grant = mint_proxy_grant(&private, &public, &claims(&public, PROXY, 3600), NOW).unwrap();
    assert!(
        verify_proxy_grant(
            &grant,
            &public,
            &crypto::b64_encode(&public),
            OTHER_PROXY,
            NOW
        )
        .is_err()
    );
}

#[test]
fn a_grant_from_another_owner_is_rejected() {
    let (public, private) = owner();
    let (other_public, _) = owner();
    let grant = mint_proxy_grant(&private, &public, &claims(&public, PROXY, 3600), NOW).unwrap();
    assert!(
        verify_proxy_grant(
            &grant,
            &other_public,
            &crypto::b64_encode(&other_public),
            PROXY,
            NOW
        )
        .is_err()
    );
}

#[test]
fn an_expired_grant_is_rejected() {
    let (public, private) = owner();
    let grant = mint_proxy_grant(&private, &public, &claims(&public, PROXY, 3600), NOW).unwrap();
    assert!(
        verify_proxy_grant(
            &grant,
            &public,
            &crypto::b64_encode(&public),
            PROXY,
            NOW + 3601
        )
        .is_err()
    );
}

#[test]
fn an_owner_cannot_mint_an_unbounded_grant() {
    let (public, private) = owner();
    let forever = claims(&public, PROXY, protocol::MAX_PROXY_GRANT_LIFETIME_SECS + 1);
    assert!(mint_proxy_grant(&private, &public, &forever, NOW).is_err());
}

#[test]
fn claims_must_match_the_signing_key() {
    let (public, private) = owner();
    let (other_public, _) = owner();
    let impersonating = claims(&other_public, PROXY, 3600);
    assert!(mint_proxy_grant(&private, &public, &impersonating, NOW).is_err());
}
