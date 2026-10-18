//! Replica-scoped enrollment/renewal owned by workload lifecycle, not by
//! scheduler membership. The selected agent issues only under owner authority.

use super::*;
use crate::store::EnrollmentBinding;
use protocol::{
    workload_connector::{ConnectorError, ConnectorRequest, inspection_challenge},
    workload_identity::{
        DEFAULT_IDENTITY_LIFETIME_SECS, EnrollmentDelegation, IDENTITY_RENEWAL_WINDOW_SECS,
        WorkloadIdentity,
    },
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

const ENROLLMENT_INTERVAL: Duration = Duration::from_secs(15);

impl AgentService {
    pub(crate) fn enrollment_supervisor(
        &self,
        cancellation: CancellationToken,
    ) -> AbortOnDropHandle<Result<()>> {
        let service = self.clone();
        AbortOnDropHandle::new(tokio::spawn(async move {
            let mut interval = tokio::time::interval(ENROLLMENT_INTERVAL);
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => return Ok(()),
                    _ = interval.tick() => {},
                    _ = service.inner.enrollment_notify.notified() => {},
                }
                let work = service.refresh_enrollments();
                tokio::select! {
                    _ = cancellation.cancelled() => return Ok(()),
                    _ = work => {},
                }
            }
        }))
    }

    async fn refresh_enrollments(&self) {
        use futures::StreamExt;
        if !self.inner.service_mesh_required {
            return;
        }
        let ids = self
            .inner
            .state
            .lock()
            .await
            .active
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        futures::stream::iter(ids)
            .for_each_concurrent(4, |id| async move {
                // Lifecycle mutation guards prevent issuing during replacement or
                // deletion. An unavailable replica is retried on a later tick.
                let Ok(_mutation) = self.mutation_with_owner(&id, MutationOwner::Enrollment) else {
                    return;
                };
                let stored = self.inner.state.lock().await.active.get(&id).cloned();
                if let Some(stored) = stored
                    && let Err(error) = self.enroll_stored(&stored).await
                {
                    log::debug!("workload enrollment refresh unavailable: {error:#}");
                }
            })
            .await;
    }

    pub(super) async fn enroll_stored(&self, stored: &StoredWorkload) -> Result<()> {
        anyhow::ensure!(
            matches!(stored.phase, StoredWorkloadPhase::Active)
                && !stored.deleting
                && !stored.runtime_id.is_empty(),
            "workload is not enrollment-ready"
        );
        let execution = self.decode_execution(&stored.grant)?;
        let delegation = EnrollmentDelegation::from_b64(&execution.workload_credential_b64)?;
        delegation.verify_scope(
            &stored.grant.namespace_id,
            &protocol::route_id(
                &crypto::b64_decode(&stored.grant.namespace_id)?,
                &stored.workload_name,
            ),
            &stored.grant.workload_id,
            Some(&self.signing_pubkey_b64()),
            Some(now_secs()),
        )?;
        let challenge = inspection_challenge(&crypto::generate_secure_nonce());
        let response = self
            .runtime_operation(
                "enrollment inspection",
                self.inner
                    .runtime
                    .connector_exchange(&stored.runtime_id, ConnectorRequest::inspect(challenge)),
            )
            .await?;
        response.validate()?;
        if response.error == Some(ConnectorError::Unsupported) {
            anyhow::bail!("connector enrollment unsupported");
        }
        let report = response
            .identity
            .context("connector enrollment unavailable")?;
        report.verify(
            &stored.grant.namespace_id,
            &stored.grant.workload_id,
            &challenge,
            now_secs(),
        )?;
        let endpoint_id = report
            .endpoint
            .endpoint_id
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let binding = EnrollmentBinding {
            workload_id: stored.grant.workload_id.clone(),
            delegation_digest: delegation.digest()?,
            endpoint_id: endpoint_id.clone(),
            signing_public_key_b64: report.endpoint.signing_pubkey.clone(),
        };
        let existing = self.inner.store.load_enrollment(&binding.workload_id)?;
        if let Some(existing) = &existing
            && existing.delegation_digest == binding.delegation_digest
        {
            anyhow::ensure!(
                existing == &binding,
                "enrolled workload keys changed; owner generation update required"
            );
        }
        // Commit the key binding BEFORE giving authority to the workload. A
        // retry after response loss cannot enroll another key in this generation.
        if existing.as_ref() != Some(&binding) {
            self.inner.store.save_enrollment(&binding)?;
        }
        let now = now_secs();
        if let Some(current) = &report.credential
            && current.delegation == delegation
            && current.claims.expires_at_secs > now.saturating_add(IDENTITY_RENEWAL_WINDOW_SECS)
        {
            current.verify_session(
                &stored.grant.namespace_id,
                &delegation.scope.manifest_id,
                &endpoint_id,
                &crypto::b64_decode(&binding.signing_public_key_b64)?,
                now,
            )?;
            return Ok(());
        }
        let lifetime = DEFAULT_IDENTITY_LIFETIME_SECS.min(delegation.scope.max_child_lifetime_secs);
        let identity = WorkloadIdentity::mint(
            &self.inner.signing_private,
            delegation,
            endpoint_id,
            binding.signing_public_key_b64,
            now,
            lifetime,
        )?;
        let installed = self
            .runtime_operation(
                "identity installation",
                self.inner.runtime.connector_exchange(
                    &stored.runtime_id,
                    ConnectorRequest::install(challenge, identity.clone()),
                ),
            )
            .await?;
        installed.validate()?;
        let report = installed
            .identity
            .context("identity installation was refused")?;
        report.verify(
            &stored.grant.namespace_id,
            &stored.grant.workload_id,
            &challenge,
            now_secs(),
        )?;
        anyhow::ensure!(
            report.credential.as_ref() == Some(&identity),
            "identity installation acknowledgement mismatch"
        );
        Ok(())
    }
}
