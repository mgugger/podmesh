use std::sync::Arc;

use anyhow::{Context, Result, anyhow, ensure};
use iroh::{
    EndpointId,
    endpoint::{RecvStream, SendStream},
};
use protocol::{
    DEFAULT_WORKLOAD_STREAM_TIMEOUT, ProxyDiscoveryRequest, ProxyEndpointDiscoveryResponse,
    SidecarRegistration, SidecarRegistrationAck, WorkloadStreamKind, read_workload_frame,
    write_workload_frame,
};

use super::tenant_gate::prove_tenant;
use super::{RuntimeState, now_millis, now_secs};

/// Largest number of distinct routing keys the proxy will hold.
pub const MAX_REGISTERED_SIDECARS: usize = 10_000;

pub async fn handle_stream(
    state: Arc<RuntimeState>,
    remote: EndpointId,
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<()> {
    let _permit = tokio::time::timeout(
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        state.stream_slots.clone().acquire_owned(),
    )
    .await
    .context("timed out waiting for workload stream capacity")?
    .context("workload stream limiter closed")?;
    let (kind, payload) = read_workload_frame(
        &mut recv,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        &state.cancellation,
    )
    .await?;
    match kind {
        WorkloadStreamKind::Handshake => {
            let verified = iroh_support::verify_workload_handshake(
                &payload,
                state.endpoint.id(),
                remote,
                protocol::machine::HandshakeRole::Request,
            )?;
            // The owner key in a handshake is a claim anybody can make. The
            // credential is what settles it: it verifies only against the
            // owner's own key, so recording the tenant here is safe.
            prove_tenant(&state, remote, &verified.handshake)?;
            let grant = verified.handshake.tenant_owner_pubkey().and_then(|owner| {
                state.grant_store.live_grant(
                    owner,
                    &state.endpoint.id().to_string(),
                    now_secs().ok()?,
                )
            });
            let encoded_grant = grant.as_deref().map(protocol::proxy_grant_to_b64);
            let response = iroh_support::build_workload_handshake_response(
                &state.identity.handshake(),
                state.endpoint.id(),
                remote,
                encoded_grant.as_deref(),
            )?;
            write_response(&state, &mut send, kind, &response).await
        }
        WorkloadStreamKind::Registration => {
            let registration =
                SidecarRegistration::from_bytes(&payload).context("decode sidecar registration")?;
            let (accepted, message) = match install_registration(&state, &registration, remote) {
                Ok(()) => (true, "ok".to_string()),
                Err(error) => {
                    log::warn!(
                        "sidecar registration from {} refused: {error:#}",
                        remote.fmt_short()
                    );
                    (false, error.to_string())
                }
            };
            let response = SidecarRegistrationAck {
                manifest_id: registration.manifest_id,
                ok: accepted,
                message,
            }
            .to_bytes()?;
            write_response(&state, &mut send, kind, &response).await
        }
        WorkloadStreamKind::ProxyDiscovery => {
            // The request names an owner, but only what this connection proved
            // decides whose proxies it may enumerate.
            let _request = ProxyDiscoveryRequest::from_bytes(&payload)?;
            let proven = state.tenants.proven(&remote);
            let endpoints = if proven.as_ref().is_some_and(|tenant| {
                state.grant_store.holds_live_grant(
                    &tenant.owner_pubkey,
                    &state.endpoint.id().to_string(),
                    now_secs().unwrap_or_default(),
                )
            }) {
                state
                    .known_proxies
                    .read()
                    .await
                    .values()
                    .filter(|record| record.endpoint_id.as_slice() != remote.as_bytes())
                    .take(_request.limit.into())
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            };
            let response = ProxyEndpointDiscoveryResponse { endpoints }.to_bytes(now_secs()?)?;
            write_response(&state, &mut send, kind, &response).await
        }
        WorkloadStreamKind::Egress => {
            super::egress::handle_egress(state, remote, send, recv, payload).await
        }
        WorkloadStreamKind::Ingress => Err(anyhow!("proxy does not accept ingress operations")),
        WorkloadStreamKind::ProxyAnnouncement => {
            let record = protocol::EndpointRecord::from_bytes(&payload, now_secs()?)?;
            ensure!(
                record.endpoint_id.as_slice() == remote.as_bytes(),
                "proxy announcement EndpointId does not match transport"
            );
            state.known_proxies.write().await.insert(remote, record);
            let own_record = state
                .own_endpoint_record
                .read()
                .map_err(|_| anyhow!("proxy EndpointRecord lock poisoned"))?
                .clone();
            let response = own_record.to_bytes(now_secs()?)?;
            write_response(&state, &mut send, kind, &response).await
        }
    }
}

/// Admit a sidecar's routes, or explain why not.
///
/// Four things must hold. The registration must name the endpoint the transport
/// authenticated; its routing key must be derived from the owner key it names;
/// the connection must already have proven that owner and that routing key with
/// an owner-signed credential; and this proxy must hold a live grant from the
/// same owner. The third is what makes the rest mean anything — without it the
/// owner key is merely asserted, and any caller could assert it.
pub(super) fn install_registration(
    state: &RuntimeState,
    registration: &SidecarRegistration,
    transport_endpoint: EndpointId,
) -> Result<()> {
    // `manifest_id == route_id(owner_pubkey, workload_name)` is enforced here.
    registration.validate()?;
    ensure!(
        registration.sidecar_peer_id == transport_endpoint.to_string(),
        "registration names an endpoint other than the authenticated transport"
    );
    // The owner named in a registration is only a claim, and the routing key is
    // derived from public inputs, so neither can carry the decision. The
    // credential presented at handshake is what proves the tenant, and the
    // registration has to match it exactly.
    let proven = state
        .tenants
        .proven(&transport_endpoint)
        .context("registration on a connection that proved no tenant")?;
    ensure!(
        proven.owner_pubkey == registration.owner_pubkey,
        "registration names a different owner than this connection proved"
    );
    ensure!(
        proven.manifest_id == registration.manifest_id,
        "registration names a different workload than this connection proved"
    );
    ensure!(
        state.grant_store.holds_live_grant(
            &registration.owner_pubkey,
            &state.endpoint.id().to_string(),
            now_secs()?,
        ),
        "this proxy holds no live owner grant for the registration tenant"
    );
    state
        .routes
        .register(registration, now_millis(), MAX_REGISTERED_SIDECARS)
}

pub(super) async fn write_response(
    state: &RuntimeState,
    send: &mut SendStream,
    kind: WorkloadStreamKind,
    payload: &[u8],
) -> Result<()> {
    write_workload_frame(
        send,
        kind,
        payload,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        &state.cancellation,
    )
    .await?;
    send.finish().context("finish workload response")?;
    Ok(())
}
