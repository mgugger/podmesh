//! Signing keys of admitted schedulers, and the endpoint each one speaks for.
//!
//! A machine relay needs a way to decide whose grants it will honour. Trusting
//! only operator-pinned peers is safe but does not scale: membership grows to
//! thousands through gossip announcements, while pinning is bounded by the
//! handful of peer URLs an operator writes down. Two schedulers that know each
//! other only by announcement, and that cannot connect directly, would then have
//! no relay willing to carry them.
//!
//! An announcement already carries both the endpoint id and the signing key, so
//! it binds the two. That binding is recorded here and used for exactly one
//! thing: letting a member authorise *itself* onto a relay.
//!
//! A binding proves nothing on its own — an endpoint record is self-signed, so
//! anyone can announce any endpoint id alongside their own key. It is safe
//! regardless, because the relay separately requires the grant's subject to be
//! the endpoint that authenticated the connection. A binding is therefore only
//! ever usable by whoever actually controls that endpoint, and can never be used
//! to vouch for a third party.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::members::MAX_CONVERGED_MEMBERS;

/// Bindings from an announced scheduler's signing key to its endpoint id.
#[derive(Clone, Debug, Default)]
pub struct MemberIssuers {
    inner: Arc<RwLock<HashMap<String, Vec<u8>>>>,
}

impl MemberIssuers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `signing_pubkey_b64` speaks for `endpoint_id`.
    ///
    /// Bounded like the member allowlist itself, and refuses rather than evicts:
    /// dropping a live binding would silently cut a scheduler off its relays,
    /// which is worse than declining to learn a new one.
    pub fn bind(&self, signing_pubkey_b64: &str, endpoint_id: Vec<u8>) {
        let Ok(mut bindings) = self.inner.write() else {
            log::error!("member issuer registry lock poisoned; binding not recorded");
            return;
        };
        if bindings.get(signing_pubkey_b64) == Some(&endpoint_id) {
            return;
        }
        if !bindings.contains_key(signing_pubkey_b64) && bindings.len() >= MAX_CONVERGED_MEMBERS {
            log::warn!("member issuer limit of {MAX_CONVERGED_MEMBERS} reached; refusing binding");
            return;
        }
        bindings.insert(signing_pubkey_b64.to_string(), endpoint_id);
    }

    /// The endpoint `signing_pubkey_b64` is allowed to authorise, if any.
    pub fn endpoint_for(&self, signing_pubkey_b64: &str) -> Option<Vec<u8>> {
        self.inner.read().ok()?.get(signing_pubkey_b64).cloned()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .read()
            .map(|bindings| bindings.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_key_authorises_nothing() {
        let issuers = MemberIssuers::new();
        assert!(issuers.endpoint_for("nobody").is_none());
    }

    #[test]
    fn a_bound_key_authorises_its_own_endpoint() {
        let issuers = MemberIssuers::new();
        issuers.bind("key-a", vec![1; 32]);
        assert_eq!(issuers.endpoint_for("key-a"), Some(vec![1; 32]));
    }

    /// A scheduler that restarts on a new endpoint keeps working.
    #[test]
    fn rebinding_a_key_replaces_its_endpoint() {
        let issuers = MemberIssuers::new();
        issuers.bind("key-a", vec![1; 32]);
        issuers.bind("key-a", vec![2; 32]);
        assert_eq!(issuers.endpoint_for("key-a"), Some(vec![2; 32]));
        assert_eq!(issuers.len(), 1);
    }

    #[test]
    fn the_registry_is_bounded() {
        let issuers = MemberIssuers::new();
        for index in 0..MAX_CONVERGED_MEMBERS + 16 {
            issuers.bind(&format!("key-{index}"), vec![1; 32]);
        }
        assert_eq!(issuers.len(), MAX_CONVERGED_MEMBERS);
    }
}
