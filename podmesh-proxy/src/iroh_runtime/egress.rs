//! Relaying a workload's outbound TCP connections.
//!
//! A tunnel spends the proxy's own network access on a tenant's behalf, so it is
//! authorised by tenant rather than by destination: the connection must have
//! proven an owner, and that owner must have granted this proxy. Addresses are
//! deliberately not filtered — podmesh runs across machines whose application
//! parts legitimately live on private networks.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use iroh::{
    EndpointId,
    endpoint::{RecvStream, SendStream},
};
use log::{debug, info};
use protocol::egress::{EgressTunnelRequest, EgressTunnelResponse};
use protocol::{DEFAULT_WORKLOAD_STREAM_TIMEOUT, WorkloadPayload, write_workload_frame};

use super::handlers::{seal_typed_response, write_typed_response};
use super::tenant_gate::authorize_egress;
use super::{ActiveGauge, RuntimeState};
use crate::egress_target;

/// Longest a tunnel may take to establish its outbound connection.
const EGRESS_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest an egress tunnel may stay open without transferring anything.
const EGRESS_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Most bytes a single egress tunnel may move in either direction.
const MAX_EGRESS_TUNNEL_BYTES: u64 = 512 * 1024 * 1024;

/// Relay a workload's outbound TCP connection.
///
/// Authorisation is by tenant, not by destination address: the proxy must hold
/// a live owner grant for the tenant behind this connection. Filtering by
/// address instead would block app parts that legitimately live on private
/// networks while leaving a granted tenant's reach unchanged.
pub(super) async fn handle_egress(
    state: Arc<RuntimeState>,
    remote: EndpointId,
    stable_id: usize,
    send: SendStream,
    recv: RecvStream,
    request: EgressTunnelRequest,
) -> Result<()> {
    let _active = ActiveGauge::new(
        state.metrics.clone(),
        state.active_egress.clone(),
        podmesh_metrics::GaugeName::ActiveEgress,
    );
    handle_egress_inner(state, remote, stable_id, send, recv, request).await
}

async fn handle_egress_inner(
    state: Arc<RuntimeState>,
    remote: EndpointId,
    stable_id: usize,
    mut send: SendStream,
    recv: RecvStream,
    request: EgressTunnelRequest,
) -> Result<()> {
    request.validate()?;
    // A tunnel spends this proxy's network access, so it is offered only to a
    // tenant that proved itself and whose owner granted this proxy.
    if let Err(error) = authorize_egress(&state, remote, stable_id) {
        let response = EgressTunnelResponse::err("destination not permitted");
        write_typed_response(&state, remote, &mut send, &response).await?;
        return Err(error);
    }

    let targets = match egress_target::resolve_target(&request.target_host, request.target_port) {
        Ok(targets) => targets,
        Err(error) => {
            log::warn!(
                "refusing egress from {} to {}:{}: {error:#}",
                remote.fmt_short(),
                request.target_host,
                request.target_port
            );
            // The peer learns only that the destination was refused, not which
            // rule refused it, so the tunnel is not usable as a port scanner.
            let response = EgressTunnelResponse::err("destination not permitted");
            write_typed_response(&state, remote, &mut send, &response).await?;
            return Err(error);
        }
    };

    info!(
        "egress tunnel endpoint={} target={}:{}",
        remote.fmt_short(),
        request.target_host,
        request.target_port
    );
    let target = match tokio::time::timeout(
        EGRESS_CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect(targets.as_slice()),
    )
    .await
    {
        Ok(Ok(target)) => target,
        // Both failure modes report the same thing for the same reason.
        Ok(Err(error)) => {
            let response = EgressTunnelResponse::err("connection failed");
            write_typed_response(&state, remote, &mut send, &response).await?;
            return Err(error).context("connect egress target");
        }
        Err(_) => {
            let response = EgressTunnelResponse::err("connection failed");
            write_typed_response(&state, remote, &mut send, &response).await?;
            return Err(anyhow!("egress target connection timed out"));
        }
    };
    let response = EgressTunnelResponse::ok();
    let envelope = seal_typed_response(&state, remote, &response)?;
    write_workload_frame(
        &mut send,
        <EgressTunnelResponse as WorkloadPayload>::TYPE.frame_kind(),
        &envelope,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        &state.cancellation,
    )
    .await?;

    let (target_read, target_write) = target.into_split();
    let stats = iroh_support::raw_relay::supervise_raw_relay(
        recv,
        send,
        target_read,
        target_write,
        iroh_support::raw_relay::RawRelayLimits {
            max_bytes_per_direction: MAX_EGRESS_TUNNEL_BYTES,
            idle_timeout: EGRESS_IDLE_TIMEOUT,
            max_lifetime: EGRESS_IDLE_TIMEOUT,
            authority_interval: Duration::from_secs(30),
        },
        state.cancellation.clone(),
        || authorize_egress(&state, remote, stable_id),
    )
    .await?;
    debug!(
        "egress tunnel closed endpoint={} sent={} received={}",
        remote.fmt_short(),
        stats.left_to_right_bytes,
        stats.right_to_left_bytes,
    );
    Ok(())
}
