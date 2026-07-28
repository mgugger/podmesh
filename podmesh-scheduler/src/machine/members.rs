//! Converging scheduler membership and relay issuer trust.
//!
//! A fresh mesh cannot know its peers' EndpointIds ahead of time: they are
//! derived from keys that only exist after a scheduler first boots. Requiring
//! every scheduler to know every peer at startup therefore either forces an
//! operator to hand-copy identifiers or deadlocks a set of schedulers that all
//! wait on each other.
//!
//! Instead the allowlist is a converging set. It starts from whatever the
//! operator configured and grows as peers are discovered, so a scheduler that
//! was unreachable at boot is admitted once it appears. The set only ever
//! grows within a hard bound, and membership alone grants nothing: a peer still
//! has to present a signed, unexpired record and authenticate its Iroh
//! connection.

use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
};

use anyhow::{Result, ensure};
use iroh::EndpointId;

/// Hard bound on the converged member set. Matches the configured member limit
/// so background discovery can never grow the allowlist past what an operator
/// could have written down by hand.
pub const MAX_CONVERGED_MEMBERS: usize = super::config::MAX_SCHEDULER_MEMBERS;

/// Hard bound on converged relay issuer keys, matching the relay's own limit.
pub const MAX_CONVERGED_ISSUERS: usize = crate::relay::MAX_TRUSTED_RELAY_ISSUERS;

/// Shared, growable set of scheduler EndpointIds allowed onto the gossip plane.
#[derive(Clone, Debug)]
pub struct MemberRegistry {
    inner: Arc<RwLock<HashSet<EndpointId>>>,
}

impl MemberRegistry {
    pub fn new(seed: HashSet<EndpointId>) -> Result<Self> {
        ensure!(
            seed.len() <= MAX_CONVERGED_MEMBERS,
            "at most {MAX_CONVERGED_MEMBERS} scheduler members are supported"
        );
        Ok(Self {
            inner: Arc::new(RwLock::new(seed)),
        })
    }

    /// Admits a peer. Returns `true` when this call actually added it, so the
    /// caller can log and dial only on a real transition.
    ///
    /// A poisoned lock is treated as "not added" rather than panicking: losing
    /// a discovery round is survivable, aborting the scheduler is not.
    pub fn insert(&self, endpoint_id: EndpointId) -> bool {
        let Ok(mut members) = self.inner.write() else {
            log::error!("scheduler member registry lock poisoned; peer not admitted");
            return false;
        };
        if members.contains(&endpoint_id) {
            return false;
        }
        if members.len() >= MAX_CONVERGED_MEMBERS {
            log::warn!(
                "scheduler member limit of {MAX_CONVERGED_MEMBERS} reached; refusing {}",
                endpoint_id.fmt_short()
            );
            return false;
        }
        members.insert(endpoint_id);
        true
    }

    pub fn contains(&self, endpoint_id: &EndpointId) -> bool {
        self.inner
            .read()
            .map(|members| members.contains(endpoint_id))
            .unwrap_or_else(|_| {
                log::error!("scheduler member registry lock poisoned; denying peer");
                false
            })
    }

    pub fn len(&self) -> usize {
        self.inner.read().map(|members| members.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Shared, growable set of Ed25519 signing keys whose relay grants this
/// scheduler's machine relay will honour.
#[derive(Clone, Debug)]
pub struct IssuerRegistry {
    inner: Arc<RwLock<Vec<Vec<u8>>>>,
}

impl IssuerRegistry {
    pub fn new(seed: Vec<Vec<u8>>) -> Result<Self> {
        ensure!(
            seed.len() <= MAX_CONVERGED_ISSUERS,
            "at most {MAX_CONVERGED_ISSUERS} relay issuer keys are supported"
        );
        Ok(Self {
            inner: Arc::new(RwLock::new(seed)),
        })
    }

    /// Trusts an additional issuer key. Returns `true` on a real transition.
    pub fn insert(&self, key: Vec<u8>) -> bool {
        let Ok(mut issuers) = self.inner.write() else {
            log::error!("relay issuer registry lock poisoned; key not trusted");
            return false;
        };
        if issuers.iter().any(|known| known == &key) {
            return false;
        }
        if issuers.len() >= MAX_CONVERGED_ISSUERS {
            log::warn!("relay issuer limit of {MAX_CONVERGED_ISSUERS} reached; refusing key");
            return false;
        }
        issuers.push(key);
        true
    }

    pub fn snapshot(&self) -> Vec<Vec<u8>> {
        self.inner
            .read()
            .map(|issuers| issuers.clone())
            .unwrap_or_else(|_| {
                log::error!("relay issuer registry lock poisoned; trusting nothing");
                Vec::new()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint_id(seed: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    #[test]
    fn a_peer_is_admitted_exactly_once() {
        let registry = MemberRegistry::new(HashSet::new()).unwrap();
        let peer = endpoint_id(1);
        assert!(registry.insert(peer));
        assert!(!registry.insert(peer));
        assert!(registry.contains(&peer));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn an_unknown_peer_is_denied() {
        let registry = MemberRegistry::new(HashSet::from([endpoint_id(1)])).unwrap();
        assert!(!registry.contains(&endpoint_id(2)));
    }

    #[test]
    fn discovery_can_never_grow_the_allowlist_without_bound() {
        let registry = MemberRegistry::new(HashSet::new()).unwrap();
        for seed in 0..=u8::MAX {
            registry.insert(endpoint_id(seed));
        }
        assert!(registry.len() <= MAX_CONVERGED_MEMBERS);
    }

    #[test]
    fn a_seed_larger_than_the_bound_is_refused() {
        let seed: HashSet<EndpointId> = (0..=u8::MAX).map(endpoint_id).collect();
        assert!(seed.len() > 8);
        assert!(IssuerRegistry::new(vec![vec![0u8; 32]; MAX_CONVERGED_ISSUERS + 1]).is_err());
    }

    #[test]
    fn an_issuer_key_is_trusted_exactly_once() {
        let registry = IssuerRegistry::new(vec![vec![1u8; 32]]).unwrap();
        assert!(!registry.insert(vec![1u8; 32]));
        assert!(registry.insert(vec![2u8; 32]));
        assert_eq!(registry.snapshot().len(), 2);
    }
}
