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

use anyhow::{Context, Result, anyhow};
use iroh::EndpointId;

/// What a connection proved about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenTenant {
    /// Base64 Ed25519 public key of the namespace owner.
    pub owner_pubkey: String,
    /// Routing key the credential was issued for.
    pub manifest_id: String,
    /// Owner-signed credential rechecked for every authorization decision.
    pub credential: Vec<u8>,
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

    /// Return a tenant only while its owner-signed workload credential remains live.
    pub fn proven_live(&self, endpoint: &EndpointId, now_secs: u64) -> Result<ProvenTenant> {
        let tenant = self
            .proven(endpoint)
            .context("operation on a connection that proved no tenant")?;
        protocol::verify_workload_credential(
            &tenant.credential,
            &tenant.owner_pubkey,
            &tenant.manifest_id,
            now_secs,
        )
        .context("workload credential is no longer valid")?;
        Ok(tenant)
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

    const NOW: u64 = 1_700_000_000;

    fn endpoint(seed: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    fn tenant(owner: &str) -> ProvenTenant {
        ProvenTenant {
            owner_pubkey: owner.into(),
            manifest_id: "manifest".into(),
            credential: vec![1, 2, 3],
        }
    }

    fn live_tenant(lifetime_secs: u64) -> ProvenTenant {
        let (public, private) = crypto::generate_signing_keypair();
        let owner = crypto::b64_encode(&public);
        let manifest_id = protocol::route_id(&public, "workload");
        let credential = protocol::mint_workload_credential(
            &private,
            &public,
            &protocol::WorkloadCredentialClaims {
                tenant_owner: owner.clone(),
                manifest_id: manifest_id.clone(),
                issued_at_secs: NOW,
                expires_at_secs: NOW + lifetime_secs,
                token_id: "tenant-session-test".into(),
            },
            NOW,
        )
        .unwrap();
        ProvenTenant {
            owner_pubkey: owner,
            manifest_id,
            credential,
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

    #[test]
    fn an_open_connection_is_refused_after_its_workload_credential_expires() {
        let sessions = TenantSessions::new();
        sessions.prove(endpoint(1), live_tenant(10)).unwrap();
        assert!(sessions.proven_live(&endpoint(1), NOW + 10).is_ok());
        assert!(sessions.proven_live(&endpoint(1), NOW + 11).is_err());
    }
}
