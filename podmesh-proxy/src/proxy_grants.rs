//! Owner-signed proxy grants held by this proxy.
//!
//! `podctl` mints a Biscuit naming this proxy and posts it here. The proxy then
//! presents it during the workload handshake so a sidecar can confirm, using
//! only its tenant owner's public key, that this proxy really was authorized to
//! front the workload.
//!
//! Anyone can post a grant, but a grant is only accepted after it verifies
//! against the owner key it claims and names this proxy's own endpoint. An
//! attacker can therefore only insert grants for an owner whose private key
//! they already hold, which no legitimate sidecar will ever ask for.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result, ensure};

/// Upper bound on distinct tenants a single proxy will hold grants for. Without
/// it, the unauthenticated submission endpoint would be an unbounded memory
/// sink.
pub const MAX_TENANT_GRANTS: usize = 1_024;

/// A grant plus the owner key it was verified against, so later checks do not
/// have to decode the key again.
#[derive(Clone, Debug)]
struct StoredGrant {
    owner_public: Vec<u8>,
    encoded: Vec<u8>,
}

#[derive(Clone, Default)]
pub struct ProxyGrantStore {
    inner: Arc<RwLock<HashMap<String, StoredGrant>>>,
}

impl ProxyGrantStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verifies that `encoded` is a live grant issued by `owner_pubkey_b64` for
    /// `local_endpoint`, then stores it.
    pub fn accept(
        &self,
        owner_pubkey_b64: &str,
        encoded: Vec<u8>,
        local_endpoint: &str,
        now_secs: u64,
    ) -> Result<()> {
        let owner_public =
            crypto::b64_decode(owner_pubkey_b64).context("decode proxy grant owner key")?;
        ensure!(
            owner_public.len() == protocol::IROH_ENDPOINT_ID_BYTES,
            "proxy grant owner key must decode to 32 bytes"
        );
        protocol::verify_proxy_grant(
            &encoded,
            &owner_public,
            owner_pubkey_b64,
            local_endpoint,
            now_secs,
        )
        .context("verify submitted proxy grant")?;

        let mut grants = self
            .inner
            .write()
            .map_err(|_| anyhow::anyhow!("proxy grant store lock poisoned"))?;
        ensure!(
            grants.contains_key(owner_pubkey_b64) || grants.len() < MAX_TENANT_GRANTS,
            "proxy grant capacity of {MAX_TENANT_GRANTS} tenants is reached"
        );
        grants.insert(
            owner_pubkey_b64.to_string(),
            StoredGrant {
                owner_public,
                encoded,
            },
        );
        Ok(())
    }

    /// Returns the stored grant for `owner_pubkey_b64` if it is still valid for
    /// this proxy, dropping it otherwise so expired grants do not accumulate.
    pub fn live_grant(
        &self,
        owner_pubkey_b64: &str,
        local_endpoint: &str,
        now_secs: u64,
    ) -> Option<Vec<u8>> {
        let stored = {
            let grants = self.inner.read().ok()?;
            grants.get(owner_pubkey_b64).cloned()?
        };
        if protocol::verify_proxy_grant(
            &stored.encoded,
            &stored.owner_public,
            owner_pubkey_b64,
            local_endpoint,
            now_secs,
        )
        .is_ok()
        {
            return Some(stored.encoded);
        }
        if let Ok(mut grants) = self.inner.write() {
            grants.remove(owner_pubkey_b64);
        }
        log::debug!("dropped an expired proxy grant for tenant {owner_pubkey_b64}");
        None
    }

    pub fn holds_live_grant(
        &self,
        owner_pubkey_b64: &str,
        local_endpoint: &str,
        now_secs: u64,
    ) -> bool {
        self.live_grant(owner_pubkey_b64, local_endpoint, now_secs)
            .is_some()
    }

    pub fn len(&self) -> usize {
        self.inner.read().map(|grants| grants.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;
    const PROXY: &str = "3f2a9c1d4b5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";

    fn grant_for(proxy: &str, lifetime: u64) -> (String, Vec<u8>, Vec<u8>) {
        let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
        let owner_b64 = crypto::b64_encode(&public);
        let encoded = protocol::mint_proxy_grant(
            &private,
            &public,
            &protocol::ProxyGrantClaims {
                tenant_owner: owner_b64.clone(),
                proxy_endpoint: proxy.to_string(),
                issued_at_secs: NOW,
                expires_at_secs: NOW + lifetime,
                token_id: "token-1".into(),
            },
            NOW,
        )
        .unwrap();
        (owner_b64, public, encoded)
    }

    #[test]
    fn a_grant_for_another_proxy_is_refused() {
        let store = ProxyGrantStore::new();
        let (owner, _, encoded) = grant_for(PROXY, 3600);
        let other = "aa2a9c1d4b5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";
        assert!(store.accept(&owner, encoded, other, NOW).is_err());
        assert!(store.is_empty());
    }

    #[test]
    fn an_accepted_grant_is_served_until_it_expires() {
        let store = ProxyGrantStore::new();
        let (owner, _, encoded) = grant_for(PROXY, 3600);
        store.accept(&owner, encoded, PROXY, NOW).unwrap();
        assert!(store.holds_live_grant(&owner, PROXY, NOW + 3599));
        assert!(!store.holds_live_grant(&owner, PROXY, NOW + 3601));
        assert!(
            store.is_empty(),
            "an expired grant must not stay resident in memory"
        );
    }

    #[test]
    fn re_submitting_a_known_tenant_never_grows_the_store() {
        let store = ProxyGrantStore::new();
        let (owner, _, encoded) = grant_for(PROXY, 3600);
        store.accept(&owner, encoded.clone(), PROXY, NOW).unwrap();
        store.accept(&owner, encoded, PROXY, NOW).unwrap();
        assert_eq!(store.len(), 1);
    }
}
