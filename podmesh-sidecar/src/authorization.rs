//! Live, endpoint-bound workload authority inside the workload boundary.
//! Public credentials can be delivered through any runtime transport; only the
//! configured owner delegation may authorize this connector's generated keys.

use anyhow::{Context, Result, ensure};
use protocol::workload_identity::{EnrollmentDelegation, WorkloadIdentity};
use tokio::sync::watch;

pub(crate) struct WorkloadAuthorization {
    delegation: EnrollmentDelegation,
    endpoint: String,
    signer: Vec<u8>,
    current: watch::Sender<Option<WorkloadIdentity>>,
}

impl WorkloadAuthorization {
    pub fn new(
        encoded: &str,
        owner: &str,
        manifest: &str,
        workload: &str,
        endpoint: String,
        signer: Vec<u8>,
    ) -> Result<Self> {
        // The two accepted formats are explicit owner enrollment delegation and
        // delegated endpoint identity. A legacy raw bearer Biscuit matches neither.
        let (delegation, initial) = match WorkloadIdentity::from_b64(encoded) {
            Ok(identity) => {
                identity.verify(None)?;
                ensure!(
                    identity.claims.endpoint_id == endpoint
                        && identity.claims.signing_public_key_b64 == crypto::b64_encode(&signer),
                    "configured identity does not name this connector"
                );
                (identity.delegation.clone(), Some(identity))
            }
            Err(_) => (
                EnrollmentDelegation::from_b64(encoded).context(
                    "endpoint enrollment authority required; bearer credentials are unsupported",
                )?,
                None,
            ),
        };
        delegation.verify_scope(owner, manifest, workload, None, None)?;
        let (current, _) = watch::channel(initial);
        Ok(Self {
            delegation,
            endpoint,
            signer,
            current,
        })
    }

    pub fn install(&self, identity: WorkloadIdentity, now: u64) -> Result<()> {
        ensure!(
            identity.delegation == self.delegation,
            "identity is not from the configured enrollment generation"
        );
        identity.verify_session(
            &self.delegation.scope.tenant_owner,
            &self.delegation.scope.manifest_id,
            &self.endpoint,
            &self.signer,
            now,
        )?;
        // Check and replace under the same watch write lock: concurrent installs
        // cannot race a newer credential into an older one.
        let mut accepted = false;
        self.current.send_if_modified(|current| {
            if current.as_ref().is_some_and(|old| {
                old.claims.issued_at_secs > identity.claims.issued_at_secs
                    || old.claims.expires_at_secs > identity.claims.expires_at_secs
            }) {
                return false;
            }
            if current.as_ref() == Some(&identity) {
                accepted = true;
                return false;
            }
            *current = Some(identity);
            accepted = true;
            true
        });
        ensure!(accepted, "identity renewal would regress authority");
        Ok(())
    }

    pub fn credential(&self, now: u64) -> Result<String> {
        let identity = self
            .current
            .borrow()
            .clone()
            .context("workload enrollment is pending")?;
        identity.verify_session(
            &self.delegation.scope.tenant_owner,
            &self.delegation.scope.manifest_id,
            &self.endpoint,
            &self.signer,
            now,
        )?;
        identity.to_b64()
    }

    pub fn current(&self) -> Option<WorkloadIdentity> {
        self.current.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Option<WorkloadIdentity>> {
        self.current.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::workload_identity::{EnrollmentScope, MAX_IDENTITY_LIFETIME_SECS};
    const NOW: u64 = 1_800_000_000;

    #[test]
    fn pending_enrollment_only_accepts_bound_generation_and_monotonic_renewal() {
        let (owner, owner_private) = crypto::generate_signing_keypair();
        let (issuer, issuer_private) = crypto::generate_signing_keypair();
        let owner_b64 = crypto::b64_encode(&owner);
        let parent = EnrollmentDelegation::mint(
            &owner_private,
            EnrollmentScope {
                tenant_owner: owner_b64.clone(),
                manifest_id: protocol::route_id(&owner, "demo"),
                workload_id: protocol::workload_id(&owner, "demo", 0),
                revision_id: protocol::revision_id(b"demo"),
                issuer_public_key_b64: crypto::b64_encode(&issuer),
                token_id: "generation-1".into(),
                issued_at_secs: NOW,
                expires_at_secs: None,
                max_child_lifetime_secs: MAX_IDENTITY_LIFETIME_SECS,
            },
            NOW,
        )
        .unwrap();
        let state = WorkloadAuthorization::new(
            &parent.to_b64().unwrap(),
            &owner_b64,
            &parent.scope.manifest_id,
            &parent.scope.workload_id,
            "a".repeat(64),
            vec![7; 32],
        )
        .unwrap();
        assert!(state.credential(NOW).is_err());
        let make = |endpoint: String, signer: Vec<u8>, now| {
            WorkloadIdentity::mint(
                &issuer_private,
                parent.clone(),
                endpoint,
                crypto::b64_encode(&signer),
                now,
                600,
            )
            .unwrap()
        };
        assert!(
            state
                .install(make("b".repeat(64), vec![7; 32], NOW), NOW)
                .is_err()
        );
        assert!(
            state
                .install(make("a".repeat(64), vec![8; 32], NOW), NOW)
                .is_err()
        );
        let first = make("a".repeat(64), vec![7; 32], NOW);
        state.install(first.clone(), NOW).unwrap();
        state.credential(NOW).unwrap();
        assert!(state.credential(NOW + 601).is_err());
        state
            .install(make("a".repeat(64), vec![7; 32], NOW + 600), NOW + 600)
            .unwrap();
        assert!(state.install(first, NOW + 600).is_err());
        assert_eq!(state.current().unwrap().claims.expires_at_secs, NOW + 1200);
        assert!(
            WorkloadAuthorization::new(
                "bearer",
                &owner_b64,
                &parent.scope.manifest_id,
                &parent.scope.workload_id,
                "a".repeat(64),
                vec![7; 32]
            )
            .is_err()
        );
    }
}
