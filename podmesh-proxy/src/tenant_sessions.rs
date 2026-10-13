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
    inner: RwLock<HashMap<EndpointId, TenantSession>>,
    metrics: podmesh_metrics::Metrics,
}

#[derive(Debug)]
struct TenantSession {
    stable_id: usize,
    tenant: Option<ProvenTenant>,
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

    pub fn with_metrics(mut self, metrics: podmesh_metrics::Metrics) -> Self {
        metrics.set_gauge(podmesh_metrics::GaugeName::TenantSessions, 0);
        self.metrics = metrics;
        self
    }

    /// Start tracking one accepted transport connection.
    pub fn begin(&self, endpoint: EndpointId, stable_id: usize) -> Result<()> {
        let mut sessions = self
            .inner
            .write()
            .map_err(|_| anyhow!("tenant session lock poisoned"))?;
        anyhow::ensure!(
            !sessions.contains_key(&endpoint),
            "endpoint already has an active connection"
        );
        sessions.insert(
            endpoint,
            TenantSession {
                stable_id,
                tenant: None,
            },
        );
        Ok(())
    }

    /// Record what `endpoint` proved exactly once for its active connection.
    pub fn prove_once(
        &self,
        endpoint: EndpointId,
        stable_id: usize,
        tenant: ProvenTenant,
    ) -> Result<()> {
        let mut sessions = self
            .inner
            .write()
            .map_err(|_| anyhow!("tenant session lock poisoned"))?;
        let session = sessions
            .get_mut(&endpoint)
            .context("connection is no longer active")?;
        anyhow::ensure!(
            session.stable_id == stable_id,
            "operation belongs to a stale connection"
        );
        anyhow::ensure!(
            session.tenant.is_none(),
            "connection already proved a tenant"
        );
        session.tenant = Some(tenant);
        let proven = sessions
            .values()
            .filter(|session| session.tenant.is_some())
            .count();
        drop(sessions);
        self.metrics
            .set_gauge(podmesh_metrics::GaugeName::TenantSessions, proven as u64);
        Ok(())
    }

    /// What the current connection proved, if anything.
    pub fn proven(&self, endpoint: &EndpointId, stable_id: usize) -> Option<ProvenTenant> {
        let sessions = self.inner.read().ok()?;
        let session = sessions.get(endpoint)?;
        (session.stable_id == stable_id)
            .then(|| session.tenant.clone())
            .flatten()
    }

    /// Install/renew only endpoint-bound authority for the same workload keys.
    pub fn prove_bound(
        &self,
        endpoint: EndpointId,
        stable_id: usize,
        tenant: ProvenTenant,
        now: u64,
    ) -> Result<()> {
        use protocol::workload_identity::WorkloadIdentity;
        let identity = WorkloadIdentity::from_bytes(&tenant.credential)?;
        identity.verify_session(
            &tenant.owner_pubkey,
            &tenant.manifest_id,
            &endpoint.to_string(),
            &crypto::b64_decode(&identity.claims.signing_public_key_b64)?,
            now,
        )?;
        let mut sessions = self
            .inner
            .write()
            .map_err(|_| anyhow!("tenant session lock poisoned"))?;
        let session = sessions
            .get_mut(&endpoint)
            .context("connection is no longer active")?;
        anyhow::ensure!(
            session.stable_id == stable_id,
            "operation belongs to a stale connection"
        );
        if let Some(old) = &session.tenant {
            let previous = WorkloadIdentity::from_bytes(&old.credential)?;
            anyhow::ensure!(
                old.owner_pubkey == tenant.owner_pubkey
                    && old.manifest_id == tenant.manifest_id
                    && previous.delegation.scope.workload_id
                        == identity.delegation.scope.workload_id
                    && previous.claims.signing_public_key_b64
                        == identity.claims.signing_public_key_b64,
                "workload renewal changes proven identity"
            );
            anyhow::ensure!(
                identity.claims.issued_at_secs >= previous.claims.issued_at_secs
                    && identity.claims.expires_at_secs >= previous.claims.expires_at_secs,
                "workload renewal regresses authority"
            );
        }
        session.tenant = Some(tenant);
        self.metrics.set_gauge(
            podmesh_metrics::GaugeName::TenantSessions,
            sessions
                .values()
                .filter(|session| session.tenant.is_some())
                .count() as u64,
        );
        Ok(())
    }

    /// Return a tenant only while its endpoint-bound credential remains live.
    pub fn proven_live(
        &self,
        endpoint: &EndpointId,
        stable_id: usize,
        now_secs: u64,
    ) -> Result<ProvenTenant> {
        let tenant = self
            .proven(endpoint, stable_id)
            .context("operation on a connection that proved no tenant")?;
        let identity =
            protocol::workload_identity::WorkloadIdentity::from_bytes(&tenant.credential)?;
        identity
            .verify_session(
                &tenant.owner_pubkey,
                &tenant.manifest_id,
                &endpoint.to_string(),
                &crypto::b64_decode(&identity.claims.signing_public_key_b64)?,
                now_secs,
            )
            .context("workload credential is no longer valid")?;
        Ok(tenant)
    }

    /// Forget a connection only if it is still the current generation.
    pub fn remove_if_current(&self, endpoint: &EndpointId, stable_id: usize) {
        if let Ok(mut sessions) = self.inner.write()
            && sessions
                .get(endpoint)
                .is_some_and(|session| session.stable_id == stable_id)
        {
            sessions.remove(endpoint);
            let proven = sessions
                .values()
                .filter(|session| session.tenant.is_some())
                .count();
            drop(sessions);
            self.metrics
                .set_gauge(podmesh_metrics::GaugeName::TenantSessions, proven as u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;
    const FIRST_CONNECTION: usize = 10;
    const SECOND_CONNECTION: usize = 20;

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
        let delegation = protocol::workload_identity::EnrollmentDelegation::mint(
            &private,
            protocol::workload_identity::EnrollmentScope {
                tenant_owner: owner.clone(),
                manifest_id: manifest_id.clone(),
                workload_id: protocol::workload_id(&public, "workload", 0),
                revision_id: protocol::revision_id(b"test"),
                issuer_public_key_b64: owner.clone(),
                token_id: "tenant-session-test".into(),
                issued_at_secs: NOW,
                expires_at_secs: None,
                max_child_lifetime_secs: 3600,
            },
            NOW,
        )
        .unwrap();
        let credential = protocol::workload_identity::WorkloadIdentity::mint(
            &private,
            delegation,
            endpoint(1).to_string(),
            owner.clone(),
            NOW,
            lifetime_secs,
        )
        .unwrap()
        .to_bytes()
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
        sessions.begin(endpoint(1), FIRST_CONNECTION).unwrap();
        assert!(sessions.proven(&endpoint(1), FIRST_CONNECTION).is_none());
    }

    #[test]
    fn a_proven_tenant_is_returned_for_its_own_connection_only() {
        let sessions = TenantSessions::new();
        sessions.begin(endpoint(1), FIRST_CONNECTION).unwrap();
        sessions
            .prove_once(endpoint(1), FIRST_CONNECTION, tenant("owner-a"))
            .unwrap();
        assert_eq!(
            sessions.proven(&endpoint(1), FIRST_CONNECTION),
            Some(tenant("owner-a"))
        );
        assert!(sessions.proven(&endpoint(2), FIRST_CONNECTION).is_none());
        assert!(sessions.proven(&endpoint(1), SECOND_CONNECTION).is_none());
    }

    #[test]
    fn proving_again_is_refused_without_replacing_the_tenant() {
        let sessions = TenantSessions::new();
        sessions.begin(endpoint(1), FIRST_CONNECTION).unwrap();
        sessions
            .prove_once(endpoint(1), FIRST_CONNECTION, tenant("owner-a"))
            .unwrap();
        assert!(
            sessions
                .prove_once(endpoint(1), FIRST_CONNECTION, tenant("owner-b"))
                .is_err()
        );
        assert_eq!(
            sessions.proven(&endpoint(1), FIRST_CONNECTION),
            Some(tenant("owner-a"))
        );
        assert_eq!(sessions.tracked(), 1);
    }

    #[test]
    fn a_closed_connection_is_forgotten() {
        let metrics = podmesh_metrics::Metrics::registered(podmesh_metrics::ComponentName::Proxy);
        let sessions = TenantSessions::new().with_metrics(metrics.clone());
        sessions.begin(endpoint(1), FIRST_CONNECTION).unwrap();
        sessions
            .prove_once(endpoint(1), FIRST_CONNECTION, tenant("owner-a"))
            .unwrap();
        assert!(metrics.snapshot().unwrap().gauges().any(|(key, value)| {
            key.gauge() == podmesh_metrics::GaugeName::TenantSessions && *value == 1
        }));
        sessions.remove_if_current(&endpoint(1), FIRST_CONNECTION);
        assert!(sessions.proven(&endpoint(1), FIRST_CONNECTION).is_none());
        assert!(metrics.snapshot().unwrap().gauges().any(|(key, value)| {
            key.gauge() == podmesh_metrics::GaugeName::TenantSessions && *value == 0
        }));
    }

    #[test]
    fn an_open_connection_is_refused_after_its_workload_credential_expires() {
        let sessions = TenantSessions::new();
        sessions.begin(endpoint(1), FIRST_CONNECTION).unwrap();
        sessions
            .prove_once(endpoint(1), FIRST_CONNECTION, live_tenant(10))
            .unwrap();
        assert!(
            sessions
                .proven_live(&endpoint(1), FIRST_CONNECTION, NOW + 10)
                .is_ok()
        );
        assert!(
            sessions
                .proven_live(&endpoint(1), FIRST_CONNECTION, NOW + 11)
                .is_err()
        );
    }

    #[test]
    fn bound_renewal_preserves_workload_keys_and_never_accepts_bearer_authority() {
        let sessions = TenantSessions::new();
        sessions.begin(endpoint(1), FIRST_CONNECTION).unwrap();
        let (public, private) = crypto::generate_signing_keypair();
        let owner = crypto::b64_encode(&public);
        let manifest = protocol::route_id(&public, "workload");
        let parent = protocol::workload_identity::EnrollmentDelegation::mint(
            &private,
            protocol::workload_identity::EnrollmentScope {
                tenant_owner: owner.clone(),
                manifest_id: manifest.clone(),
                workload_id: protocol::workload_id(&public, "workload", 0),
                revision_id: protocol::revision_id(b"test"),
                issuer_public_key_b64: owner.clone(),
                token_id: "renewal-test".into(),
                issued_at_secs: NOW,
                expires_at_secs: None,
                max_child_lifetime_secs: 3600,
            },
            NOW,
        )
        .unwrap();
        let make = |at, signer: String, peer: EndpointId| ProvenTenant {
            owner_pubkey: owner.clone(),
            manifest_id: manifest.clone(),
            credential: protocol::workload_identity::WorkloadIdentity::mint(
                &private,
                parent.clone(),
                peer.to_string(),
                signer,
                at,
                600,
            )
            .unwrap()
            .to_bytes()
            .unwrap(),
        };
        sessions
            .prove_bound(
                endpoint(1),
                FIRST_CONNECTION,
                make(NOW, owner.clone(), endpoint(1)),
                NOW,
            )
            .unwrap();
        sessions
            .prove_bound(
                endpoint(1),
                FIRST_CONNECTION,
                make(NOW + 300, owner.clone(), endpoint(1)),
                NOW + 300,
            )
            .unwrap();
        sessions
            .proven_live(&endpoint(1), FIRST_CONNECTION, NOW + 800)
            .unwrap();
        assert!(
            sessions
                .prove_bound(
                    endpoint(1),
                    FIRST_CONNECTION,
                    make(NOW, owner.clone(), endpoint(1)),
                    NOW + 300
                )
                .is_err()
        );
        assert!(
            sessions
                .prove_bound(
                    endpoint(1),
                    FIRST_CONNECTION,
                    make(NOW + 301, crypto::b64_encode(&[8; 32]), endpoint(1)),
                    NOW + 301
                )
                .is_err()
        );
        assert!(
            sessions
                .prove_bound(
                    endpoint(1),
                    FIRST_CONNECTION,
                    make(NOW + 301, owner.clone(), endpoint(2)),
                    NOW + 301
                )
                .is_err()
        );
        assert!(
            sessions
                .prove_bound(endpoint(1), FIRST_CONNECTION, tenant("bearer"), NOW)
                .is_err()
        );
    }

    #[test]
    fn stale_generation_cannot_prove_read_or_remove_current_state() {
        let sessions = TenantSessions::new();
        sessions.begin(endpoint(1), FIRST_CONNECTION).unwrap();
        assert!(
            sessions
                .prove_once(endpoint(1), SECOND_CONNECTION, tenant("stale"))
                .is_err()
        );
        sessions
            .prove_once(endpoint(1), FIRST_CONNECTION, tenant("owner-a"))
            .unwrap();
        sessions.remove_if_current(&endpoint(1), SECOND_CONNECTION);
        assert_eq!(
            sessions.proven(&endpoint(1), FIRST_CONNECTION),
            Some(tenant("owner-a"))
        );
        assert!(sessions.proven(&endpoint(1), SECOND_CONNECTION).is_none());

        sessions.remove_if_current(&endpoint(1), FIRST_CONNECTION);
        sessions.begin(endpoint(1), SECOND_CONNECTION).unwrap();
        assert!(sessions.proven(&endpoint(1), SECOND_CONNECTION).is_none());
    }

    #[test]
    fn duplicate_active_endpoint_is_refused_until_current_removal() {
        let sessions = TenantSessions::new();
        sessions.begin(endpoint(1), FIRST_CONNECTION).unwrap();
        assert!(sessions.begin(endpoint(1), SECOND_CONNECTION).is_err());
        sessions.remove_if_current(&endpoint(1), FIRST_CONNECTION);
        sessions.begin(endpoint(1), SECOND_CONNECTION).unwrap();
    }
}
