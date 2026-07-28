use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, ensure};
use iroh::{
    EndpointId,
    endpoint::{RecvStream, SendStream},
};
use log::{debug, info};
use protocol::egress::{EgressTunnelRequest, EgressTunnelResponse};
use protocol::{
    DEFAULT_WORKLOAD_STREAM_TIMEOUT, ProxyDiscoveryRequest, ProxyEndpointDiscoveryResponse,
    SidecarRegistration, SidecarRegistrationAck, WorkloadStreamKind, read_workload_frame,
    write_workload_frame,
};

use super::{RuntimeState, SidecarRouteEntry, now_millis, now_secs};
use crate::restapi::ProxyGrantStore;

const EGRESS_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REGISTERED_SIDECARS: usize = 10_000;

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
            let verified = iroh_support::verify_workload_handshake(&payload, remote)?;
            let grant = verified.handshake.tenant_owner_pubkey().and_then(|owner| {
                state.grant_store.live_grant(
                    owner,
                    &state.endpoint.id().to_string(),
                    now_secs().ok()?,
                )
            });
            let encoded_grant = grant.as_deref().map(protocol::proxy_grant_to_b64);
            let response = iroh_support::build_workload_handshake_response(
                state.endpoint.id(),
                encoded_grant.as_deref(),
            )?;
            write_response(&state, &mut send, kind, &response).await
        }
        WorkloadStreamKind::Registration => {
            let registration =
                SidecarRegistration::from_bytes(&payload).context("decode sidecar registration")?;
            let (mut accepted, mut message) = evaluate_sidecar_registration(
                &registration,
                remote,
                &state.grant_store,
                &state.endpoint.id().to_string(),
                now_secs()?,
            );
            if accepted {
                let mut routes = state
                    .routing_table
                    .write()
                    .map_err(|_| anyhow!("routing table lock poisoned"))?;
                if !routes.contains_key(&registration.manifest_id)
                    && routes.len() >= MAX_REGISTERED_SIDECARS
                {
                    accepted = false;
                    message = "proxy sidecar route capacity reached".into();
                } else {
                    routes.insert(
                        registration.manifest_id.clone(),
                        SidecarRouteEntry {
                            sidecar_peer_id: remote.to_string(),
                            routes: registration.routes.clone(),
                            registered_at: now_millis(),
                        },
                    );
                }
            }
            let response = SidecarRegistrationAck {
                manifest_id: registration.manifest_id,
                ok: accepted,
                message,
            }
            .to_bytes();
            write_response(&state, &mut send, kind, &response).await
        }
        WorkloadStreamKind::ProxyDiscovery => {
            let request = ProxyDiscoveryRequest::from_bytes(&payload)?;
            let endpoints = if state.grant_store.holds_live_grant(
                &request.owner_pubkey,
                &state.endpoint.id().to_string(),
                now_secs()?,
            ) {
                state
                    .known_proxies
                    .read()
                    .await
                    .values()
                    .filter(|record| record.endpoint_id.as_slice() != remote.as_bytes())
                    .take(usize::from(request.limit))
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            };
            let response = ProxyEndpointDiscoveryResponse { endpoints }.to_bytes(now_secs()?)?;
            write_response(&state, &mut send, kind, &response).await
        }
        WorkloadStreamKind::Egress => handle_egress(state, remote, send, recv, payload).await,
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

pub fn evaluate_sidecar_registration(
    registration: &SidecarRegistration,
    transport_endpoint: EndpointId,
    grant_store: &ProxyGrantStore,
    local_endpoint: &str,
    now_secs: u64,
) -> (bool, String) {
    if registration.sidecar_signing_pubkey.is_empty() {
        return (false, "missing sidecar_signing_pubkey".into());
    }
    let signed_data = format!(
        "{}{}",
        registration.manifest_id, registration.sidecar_peer_id
    );
    let signature_valid = crypto::b64_decode(&registration.sidecar_signing_pubkey)
        .and_then(|public_key| {
            let signature = crypto::b64_decode(&registration.sig)?;
            crypto::verify_envelope(&public_key, signed_data.as_bytes(), &signature)
        })
        .is_ok();
    if !signature_valid {
        return (false, "signature verification failed".into());
    }
    if registration.sidecar_peer_id != transport_endpoint.to_string() {
        return (
            false,
            "transport EndpointId does not match registration".into(),
        );
    }
    if !grant_store.holds_live_grant(&registration.owner_pubkey, local_endpoint, now_secs) {
        return (
            false,
            "this proxy holds no live owner grant for the registration tenant".into(),
        );
    }
    (true, "ok".into())
}

async fn write_response(
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

async fn handle_egress(
    state: Arc<RuntimeState>,
    remote: EndpointId,
    mut send: SendStream,
    mut recv: RecvStream,
    payload: Vec<u8>,
) -> Result<()> {
    let request: EgressTunnelRequest =
        postcard::from_bytes(&payload).context("decode egress request")?;
    ensure!(request.protocol == "tcp", "unsupported egress protocol");
    ensure!(
        !request.target_host.is_empty(),
        "egress target host is empty"
    );
    info!(
        "egress tunnel request endpoint={} target={}:{}",
        remote.fmt_short(),
        request.target_host,
        request.target_port
    );
    let target = match tokio::time::timeout(
        EGRESS_CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((request.target_host.as_str(), request.target_port)),
    )
    .await
    {
        Ok(Ok(target)) => target,
        Ok(Err(error)) => {
            let response = postcard::to_allocvec(&EgressTunnelResponse::err(format!(
                "connection failed: {error}"
            )))?;
            write_response(&state, &mut send, WorkloadStreamKind::Egress, &response).await?;
            return Err(error).context("connect egress target");
        }
        Err(_) => {
            let response = postcard::to_allocvec(&EgressTunnelResponse::err("connection timeout"))?;
            write_response(&state, &mut send, WorkloadStreamKind::Egress, &response).await?;
            return Err(anyhow!("egress target connection timed out"));
        }
    };
    let response = postcard::to_allocvec(&EgressTunnelResponse::ok())?;
    write_workload_frame(
        &mut send,
        WorkloadStreamKind::Egress,
        &response,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        &state.cancellation,
    )
    .await?;
    let (mut target_read, mut target_write) = target.into_split();
    let client_to_target = async {
        let bytes = tokio::io::copy(&mut recv, &mut target_write).await?;
        tokio::io::AsyncWriteExt::shutdown(&mut target_write).await?;
        Ok::<u64, std::io::Error>(bytes)
    };
    let target_to_client = tokio::io::copy(&mut target_read, &mut send);
    let (sent, received) = tokio::try_join!(client_to_target, target_to_client)?;
    send.finish().context("finish egress response stream")?;
    debug!(
        "egress tunnel closed endpoint={} sent={} received={}",
        remote.fmt_short(),
        sent,
        received
    );
    Ok(())
}
