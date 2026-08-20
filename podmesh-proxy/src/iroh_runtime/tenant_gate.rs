//! Deciding which tenant a workload connection belongs to, and what it may do.
//!
//! Kept apart from stream handling because these are the checks that make every
//! other operation on a connection meaningful: without a proven tenant, an owner
//! key is only a claim, and the key is public.

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;

use super::{RuntimeState, now_secs};

/// Record the tenant a handshake proved, or refuse the handshake.
///
/// The credential is verified against the owner key the handshake names. That
/// is safe despite the name being attacker-chosen, because the same key is the
/// Biscuit root: a credential only verifies if the owner's private key signed
/// it, which an impostor does not have.
pub(super) fn prove_tenant(
    state: &RuntimeState,
    remote: EndpointId,
    handshake: &protocol::machine::Handshake,
) -> Result<()> {
    let owner = handshake
        .tenant_owner_pubkey()
        .context("workload handshake did not name a tenant owner")?;
    let manifest_id = handshake
        .manifest_id()
        .context("workload handshake did not name a workload")?;
    let encoded = handshake
        .workload_credential_b64()
        .context("workload handshake did not include an owner-signed credential")?;
    let credential = protocol::workload_credential_from_b64(encoded)?;
    protocol::verify_workload_credential(&credential, owner, manifest_id, now_secs()?)
        .context("verify workload credential")?;
    state.tenants.prove(
        remote,
        crate::tenant_sessions::ProvenTenant {
            owner_pubkey: owner.to_string(),
            manifest_id: manifest_id.to_string(),
            credential,
        },
    )
}

pub(super) fn live_tenant(
    state: &RuntimeState,
    remote: EndpointId,
) -> Result<crate::tenant_sessions::ProvenTenant> {
    state.tenants.proven_live(&remote, now_secs()?)
}

/// Decide whether this connection may open an egress tunnel.
pub(super) fn authorize_egress(state: &RuntimeState, remote: EndpointId) -> Result<()> {
    let proven = live_tenant(state, remote)?;
    ensure!(
        state.grant_store.holds_live_grant(
            &proven.owner_pubkey,
            &state.endpoint.id().to_string(),
            now_secs()?,
        ),
        "this proxy holds no live owner grant for the egress tenant"
    );
    Ok(())
}
