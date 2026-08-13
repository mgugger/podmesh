//! Relaying a workload's outbound TCP connections.
//!
//! A tunnel spends the proxy's own network access on a tenant's behalf, so it is
//! authorised by tenant rather than by destination: the connection must have
//! proven an owner, and that owner must have granted this proxy. Addresses are
//! deliberately not filtered — podmesh runs across machines whose application
//! parts legitimately live on private networks.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, ensure};
use iroh::{
    EndpointId,
    endpoint::{RecvStream, SendStream},
};
use log::{debug, info};
use protocol::egress::{EgressTunnelRequest, EgressTunnelResponse};
use protocol::{DEFAULT_WORKLOAD_STREAM_TIMEOUT, WorkloadStreamKind, write_workload_frame};
use tokio::io::AsyncReadExt;

use super::RuntimeState;
use super::handlers::write_response;
use super::tenant_gate::authorize_egress;
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
    mut send: SendStream,
    recv: RecvStream,
    payload: Vec<u8>,
) -> Result<()> {
    let request: EgressTunnelRequest =
        postcard::from_bytes(&payload).context("decode egress request")?;
    ensure!(request.protocol == "tcp", "unsupported egress protocol");
    // A tunnel spends this proxy's network access, so it is offered only to a
    // tenant that proved itself and whose owner granted this proxy.
    if let Err(error) = authorize_egress(&state, remote) {
        let response =
            postcard::to_allocvec(&EgressTunnelResponse::err("destination not permitted"))?;
        write_response(&state, &mut send, WorkloadStreamKind::Egress, &response).await?;
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
            let response =
                postcard::to_allocvec(&EgressTunnelResponse::err("destination not permitted"))?;
            write_response(&state, &mut send, WorkloadStreamKind::Egress, &response).await?;
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
            let response = postcard::to_allocvec(&EgressTunnelResponse::err("connection failed"))?;
            write_response(&state, &mut send, WorkloadStreamKind::Egress, &response).await?;
            return Err(error).context("connect egress target");
        }
        Err(_) => {
            let response = postcard::to_allocvec(&EgressTunnelResponse::err("connection failed"))?;
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

    let (target_read, mut target_write) = target.into_split();
    let mut bounded_recv = recv.take(MAX_EGRESS_TUNNEL_BYTES);
    let mut bounded_target = target_read.take(MAX_EGRESS_TUNNEL_BYTES);
    let client_to_target = async {
        let bytes = tokio::io::copy(&mut bounded_recv, &mut target_write).await?;
        tokio::io::AsyncWriteExt::shutdown(&mut target_write).await?;
        Ok::<u64, std::io::Error>(bytes)
    };
    let target_to_client = tokio::io::copy(&mut bounded_target, &mut send);
    // A tunnel that neither side ever closes would hold a stream permit
    // forever, so the whole relay is bounded in time as well as in bytes.
    let (sent, received) = tokio::time::timeout(
        EGRESS_IDLE_TIMEOUT,
        futures::future::try_join(client_to_target, target_to_client),
    )
    .await
    .context("egress tunnel exceeded its maximum lifetime")??;
    send.finish().context("finish egress response stream")?;
    debug!(
        "egress tunnel closed endpoint={} sent={sent} received={received}",
        remote.fmt_short()
    );
    Ok(())
}
