use protocol::{ProxyGrantClaims, mint_proxy_grant, verify_proxy_grant};

const NOW: u64 = 1_700_000_000;
const PROXY: &str = "3f2a9c1d4b5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";
const OTHER_PROXY: &str = "aa2a9c1d4b5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";

fn owner() -> (Vec<u8>, Vec<u8>) {
    let (public, private) = crypto::generate_signing_keypair();
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

/// Biscuit's default authorizer gives up after a millisecond of wall clock, so
/// a valid grant would be refused whenever the machine is busy — precisely when
/// a proxy is under load. Verification must depend on the token, not on how
/// contended the host is.
#[test]
fn verification_does_not_depend_on_machine_load() {
    let (owner_public, owner_private) = crypto::generate_signing_keypair();
    let owner_b64 = crypto::b64_encode(&owner_public);
    let grant = mint_proxy_grant(
        &owner_private,
        &owner_public,
        &ProxyGrantClaims {
            tenant_owner: owner_b64.clone(),
            proxy_endpoint: PROXY.into(),
            issued_at_secs: NOW,
            expires_at_secs: NOW + 3600,
            token_id: "load-test".into(),
        },
        NOW,
    )
    .expect("mint");

    // Saturate the machine while verifying, so any wall-clock-sensitive bound
    // would trip.
    let threads: Vec<_> = (0..std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4))
        .map(|_| {
            std::thread::spawn(|| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
                let mut spin = 0u64;
                while std::time::Instant::now() < deadline {
                    spin = spin.wrapping_add(1);
                }
                spin
            })
        })
        .collect();

    for _ in 0..200 {
        verify_proxy_grant(&grant, &owner_public, &owner_b64, PROXY, NOW)
            .expect("a valid grant must verify regardless of machine load");
    }

    for thread in threads {
        let _ = thread.join();
    }
}
