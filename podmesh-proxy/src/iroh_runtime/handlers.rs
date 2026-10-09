use std::{sync::Arc, time::Instant};

use anyhow::{Context, Result, anyhow, ensure};
use iroh::{
    EndpointId,
    endpoint::{RecvStream, SendStream},
};
use protocol::{
    DEFAULT_WORKLOAD_STREAM_TIMEOUT, ProxyDiscoveryRequest, ProxyEndpointDiscoveryResponse,
    SidecarRegistration, SidecarRegistrationAck, WorkloadEnvelopeParts, WorkloadPayload,
    WorkloadStreamKind, accept_workload_payload, read_workload_frame, seal_workload_payload,
    write_workload_frame,
};

use super::tenant_gate::{live_tenant, prove_tenant};
use super::{RuntimeState, finish_operation, now_millis, now_secs};
use tokio::sync::Semaphore;

/// Largest number of distinct routing keys the proxy will hold.
pub const MAX_REGISTERED_SIDECARS: usize = 10_000;

pub async fn handle_stream(
    state: Arc<RuntimeState>,
    remote: EndpointId,
    stable_id: usize,
    connection_slots: Arc<Semaphore>,
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<()> {
    let _connection_permit = match connection_slots.try_acquire_owned() {
        Ok(permit) => permit,
        Err(error) => {
            state
                .metrics
                .record_event(podmesh_metrics::EventName::StreamSaturation);
            return Err(error).context("connection stream limit reached");
        }
    };
    let permit = tokio::time::timeout(
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        state.stream_slots.clone().acquire_owned(),
    )
    .await;
    let _permit = match permit {
        Ok(Ok(permit)) => permit,
        Ok(Err(error)) => {
            state
                .metrics
                .record_event(podmesh_metrics::EventName::StreamSaturation);
            return Err(error).context("workload stream limiter closed");
        }
        Err(error) => {
            state
                .metrics
                .record_event(podmesh_metrics::EventName::StreamSaturation);
            return Err(error).context("timed out waiting for workload stream capacity");
        }
    };
    let (kind, payload) = read_workload_frame(
        &mut recv,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        &state.cancellation,
    )
    .await?;
    let operation = match kind {
        WorkloadStreamKind::Handshake => podmesh_metrics::OperationName::WorkloadHandshake,
        WorkloadStreamKind::Registration => podmesh_metrics::OperationName::Registration,
        WorkloadStreamKind::ProxyDiscovery => podmesh_metrics::OperationName::Discovery,
        WorkloadStreamKind::Egress => podmesh_metrics::OperationName::Egress,
        WorkloadStreamKind::Ingress => podmesh_metrics::OperationName::Ingress,
        WorkloadStreamKind::ProxyAnnouncement => podmesh_metrics::OperationName::PeerAnnouncement,
    };
    let timer = state.metrics.operation_started(operation);
    let mut terminal = None;
    let result = async {
        match kind {
            WorkloadStreamKind::Handshake => {
                let accepted = accept_typed_request::<protocol::machine::WorkloadHandshakeRequest>(
                    &state, remote, &payload,
                )?;
                // The owner key in a handshake is a claim anybody can make. The
                // credential is what settles it: it verifies only against the
                // owner's own key, so recording the tenant here is safe.
                prove_tenant(&state, remote, stable_id, &accepted.payload)?;
                let grant = state.grant_store.live_grant(
                    &accepted.payload.tenant_owner_pubkey,
                    &state.endpoint.id().to_string(),
                    now_secs()?,
                );
                let encoded_grant = grant.as_deref().map(protocol::proxy_grant_to_b64);
                let response =
                    protocol::machine::WorkloadHandshakeResponse::new(encoded_grant.as_deref());
                write_typed_response(&state, remote, &mut send, &response).await
            }
            WorkloadStreamKind::Registration => {
                let accepted =
                    accept_typed_request::<SidecarRegistration>(&state, remote, &payload)?;
                let registration = accepted.payload;
                let (accepted, message) =
                    match install_registration(&state, &registration, remote, stable_id) {
                        Ok(()) => {
                            terminal = Some((
                                podmesh_metrics::Outcome::Success,
                                podmesh_metrics::Reason::None,
                            ));
                            (true, "ok".to_string())
                        }
                        Err(error) => {
                            terminal = Some((
                                podmesh_metrics::Outcome::Refused,
                                podmesh_metrics::Reason::Authorization,
                            ));
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
                };
                write_typed_response(&state, remote, &mut send, &response).await
            }
            WorkloadStreamKind::ProxyDiscovery => {
                // The request names an owner, but only what this connection proved
                // decides whose proxies it may enumerate.
                let accepted =
                    accept_typed_request::<ProxyDiscoveryRequest>(&state, remote, &payload)?;
                let request = accepted.payload;
                let proven = live_tenant(&state, remote, stable_id).ok();
                let authorized = proven.as_ref().is_some_and(|tenant| {
                    state.grant_store.holds_live_grant(
                        &tenant.owner_pubkey,
                        &state.endpoint.id().to_string(),
                        now_secs().unwrap_or_default(),
                    )
                });
                let endpoints = if authorized {
                    state
                        .known_proxies
                        .read()
                        .await
                        .values()
                        .filter(|record| record.endpoint_id.as_slice() != remote.as_bytes())
                        .take(request.limit.into())
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
                terminal = Some(if authorized {
                    (
                        podmesh_metrics::Outcome::Success,
                        podmesh_metrics::Reason::None,
                    )
                } else {
                    (
                        podmesh_metrics::Outcome::Refused,
                        podmesh_metrics::Reason::Authorization,
                    )
                });
                let response = ProxyEndpointDiscoveryResponse { endpoints };
                write_typed_response(&state, remote, &mut send, &response).await
            }
            WorkloadStreamKind::Egress => {
                let accepted = accept_typed_request::<protocol::egress::EgressTunnelRequest>(
                    &state, remote, &payload,
                )?;
                super::egress::handle_egress(
                    state.clone(),
                    remote,
                    stable_id,
                    send,
                    recv,
                    accepted.payload,
                )
                .await
            }
            WorkloadStreamKind::Ingress => Err(anyhow!("proxy does not accept ingress operations")),
            WorkloadStreamKind::ProxyAnnouncement => {
                let accepted = accept_typed_request::<protocol::ProxyAnnouncementRequest>(
                    &state, remote, &payload,
                )?;
                let record = accepted.payload.endpoint;
                ensure!(
                    record.endpoint_id.as_slice() == remote.as_bytes(),
                    "proxy announcement EndpointId does not match transport"
                );
                ensure!(
                    accepted.sender_signing_key
                        == crypto::b64_decode(&record.signing_pubkey)
                            .context("decode proxy announcement signing key")?,
                    "proxy announcement envelope key does not match endpoint record"
                );
                state.known_proxies.write().await.insert(remote, record);
                state.metrics.set_gauge(
                    podmesh_metrics::GaugeName::ProxyPeers,
                    state.known_proxies.read().await.len() as u64,
                );
                let own_record = state
                    .own_endpoint_record
                    .read()
                    .map_err(|_| anyhow!("proxy EndpointRecord lock poisoned"))?
                    .clone();
                let response = protocol::ProxyAnnouncementResponse {
                    endpoint: own_record,
                };
                write_typed_response(&state, remote, &mut send, &response).await
            }
        }
    }
    .await;
    if let Some((outcome, reason)) = terminal {
        timer.finish(outcome, reason);
    } else {
        finish_operation(timer, &result, &state.metrics);
    }
    result
}

fn accept_typed_request<T: WorkloadPayload>(
    state: &RuntimeState,
    remote: EndpointId,
    payload: &[u8],
) -> Result<protocol::AcceptedWorkloadPayload<T>> {
    accept_workload_payload(
        payload,
        &state.endpoint.id().to_string(),
        &remote.to_string(),
        now_millis(),
        &state.replay_registry,
        Instant::now(),
    )
}

pub(super) async fn write_typed_response<T: WorkloadPayload>(
    state: &RuntimeState,
    remote: EndpointId,
    send: &mut SendStream,
    payload: &T,
) -> Result<()> {
    let envelope = seal_typed_response(state, remote, payload)?;
    write_response(state, send, T::TYPE.frame_kind(), &envelope).await
}

pub(super) fn seal_typed_response<T: WorkloadPayload>(
    state: &RuntimeState,
    remote: EndpointId,
    payload: &T,
) -> Result<Vec<u8>> {
    seal_workload_payload(
        WorkloadEnvelopeParts {
            nonce: &crypto::generate_secure_nonce(),
            now_millis: now_millis(),
            sender_id: &state.endpoint.id().to_string(),
            recipient_id: &remote.to_string(),
            sender_signing_public: state.identity.signing_public(),
            sender_signing_private: state.identity.signing_private(),
            sender_kem_public: Some(state.identity.kem_public()),
        },
        payload,
    )
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
    stable_id: usize,
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
    let proven = live_tenant(state, transport_endpoint, stable_id)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use podmesh_metrics::{ComponentName, EventName, Metrics, OperationName, Outcome, Reason};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn invalid_and_replayed_envelopes_are_classified_before_timer_drop() -> Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let metrics = Metrics::registered(ComponentName::Proxy);
            let config = crate::config::Config {
                proxy_endpoints: Vec::new(),
                workload_replay_limits: protocol::ReplayLimits::default(),
                identity: crate::config::IdentitySource::Ephemeral,
                iroh_bind_addr: "127.0.0.1:0".parse()?,
                metrics_listen: None,
                workload_relay: None,
                workload_relay_certificate_der: Vec::new(),
                publish_relay_bootstrap: false,
                rest_host: "127.0.0.1".into(),
                rest_port: 0,
                disable_rest_api: true,
                enable_ingress: false,
                owner_pubkey: None,
                advertise_addresses: Vec::new(),
                rest_rate_limit_per_minute: 0,
            };
            let node = super::super::spawn_with_metrics(&config, metrics.clone()).await?;
            let client = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
                .bind_addr("127.0.0.1:0".parse::<std::net::SocketAddr>()?)?
                .clear_relay_transports()
                .bind()
                .await?;
            let connection = client
                .connect(node.endpoint.addr(), protocol::WORKLOAD_ALPN)
                .await?;
            let identity = iroh_support::NodeIdentity::ephemeral();
            let request = ProxyDiscoveryRequest {
                owner_pubkey: "test-owner".into(),
                limit: 1,
            };
            let envelope = seal_workload_payload(
                WorkloadEnvelopeParts {
                    nonce: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    now_millis: now_millis(),
                    sender_id: &client.id().to_string(),
                    recipient_id: &node.endpoint.id().to_string(),
                    sender_signing_public: identity.signing_public(),
                    sender_signing_private: identity.signing_private(),
                    sender_kem_public: None,
                },
                &request,
            )?;
            for payload in [&b"invalid"[..], envelope.as_slice(), envelope.as_slice()] {
                let (mut send, mut recv) = connection.open_bi().await?;
                write_workload_frame(
                    &mut send,
                    WorkloadStreamKind::ProxyDiscovery,
                    payload,
                    DEFAULT_WORKLOAD_STREAM_TIMEOUT,
                    &CancellationToken::new(),
                )
                .await?;
                send.finish()?;
                let _ = recv.read_to_end(128 * 1024).await;
            }
            loop {
                let snapshot = metrics.snapshot().unwrap();
                let count: u64 = snapshot
                    .operations()
                    .filter(|(key, _)| key.operation() == OperationName::Discovery)
                    .map(|(_, value)| value.count())
                    .sum();
                if count == 3 {
                    for reason in [Reason::Invalid, Reason::Replay, Reason::Authorization] {
                        ensure!(snapshot.operations().any(|(key, value)| key.operation()
                            == OperationName::Discovery
                            && key.outcome() == Outcome::Refused
                            && key.reason() == reason
                            && value.count() == 1));
                    }
                    ensure!(
                        snapshot
                            .events()
                            .any(|(key, value)| key.event() == EventName::ReplayRefusal
                                && *value == 1)
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
            client.close().await;
            node.shutdown().await;
            Ok(())
        })
        .await
        .context("proxy metrics regression timed out")?
    }
}
