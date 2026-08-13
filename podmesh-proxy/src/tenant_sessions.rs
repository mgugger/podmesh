//! Which tenant each open workload connection has proven itself to belong to.
//!
//! A proxy cannot take a caller's word for its tenant: the owner's public key is
//! public, so anyone can name it. A connection is therefore recorded here only
//! after it presents a credential signed by that owner's private key, and every
//! later operation on that connection is checked against what was proven rather
//! than against anything the caller repeats.
//!
//! The entry lives as long as the connection. Nothing is remembered after it
//! closes, so a reconnecting sidecar proves itself again.

use std::collections::HashMap;
use std::sync::RwLock;

use anyhow::{Result, anyhow};
use iroh::EndpointId;

/// What a connection proved about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenTenant {
    /// Base64 Ed25519 public key of the namespace owner.
    pub owner_pubkey: String,
    /// Routing key the credential was issued for.
    pub manifest_id: String,
}

/// Proven tenancy per open connection.
#[derive(Default)]
pub struct TenantSessions {
    inner: RwLock<HashMap<EndpointId, ProvenTenant>>,
}

impl TenantSessions {
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.inner
            .read()
            .map(|sessions| sessions.len())
            .unwrap_or(0)
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Record what `endpoint` proved. A later handshake on the same connection
    /// replaces the entry, so a sidecar cannot accumulate tenancies.
    pub fn prove(&self, endpoint: EndpointId, tenant: ProvenTenant) -> Result<()> {
        self.inner
            .write()
            .map_err(|_| anyhow!("tenant session lock poisoned"))?
            .insert(endpoint, tenant);
        Ok(())
    }

    /// What `endpoint` proved, if anything.
    pub fn proven(&self, endpoint: &EndpointId) -> Option<ProvenTenant> {
        self.inner.read().ok()?.get(endpoint).cloned()
    }

    /// Forget a closed connection.
    pub fn forget(&self, endpoint: &EndpointId) {
        if let Ok(mut sessions) = self.inner.write() {
            sessions.remove(endpoint);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(seed: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    fn tenant(owner: &str) -> ProvenTenant {
        ProvenTenant {
            owner_pubkey: owner.into(),
            manifest_id: "manifest".into(),
        }
    }

    #[test]
    fn a_connection_that_proved_nothing_has_no_tenant() {
        let sessions = TenantSessions::new();
        assert!(sessions.proven(&endpoint(1)).is_none());
    }

    #[test]
    fn a_proven_tenant_is_returned_for_its_own_connection_only() {
        let sessions = TenantSessions::new();
        sessions.prove(endpoint(1), tenant("owner-a")).unwrap();
        assert_eq!(sessions.proven(&endpoint(1)), Some(tenant("owner-a")));
        assert!(sessions.proven(&endpoint(2)).is_none());
    }

    /// A second handshake replaces rather than adds, so one connection can
    /// never be treated as belonging to two tenants at once.
    #[test]
    fn proving_again_replaces_the_previous_tenant() {
        let sessions = TenantSessions::new();
        sessions.prove(endpoint(1), tenant("owner-a")).unwrap();
        sessions.prove(endpoint(1), tenant("owner-b")).unwrap();
        assert_eq!(sessions.proven(&endpoint(1)), Some(tenant("owner-b")));
        assert_eq!(sessions.tracked(), 1);
    }

    #[test]
    fn a_closed_connection_is_forgotten() {
        let sessions = TenantSessions::new();
        sessions.prove(endpoint(1), tenant("owner-a")).unwrap();
        sessions.forget(&endpoint(1));
        assert!(sessions.proven(&endpoint(1)).is_none());
    }
}
