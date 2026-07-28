//! One-hop scheduler-to-scheduler relay of owner control traffic.
//!
//! An agent holds exactly one scheduler attachment, but `podctl` is expected to
//! be able to talk to any scheduler it can reach. This module closes that gap:
//! a scheduler that does not hold the target attachment asks its admitted peers
//! which one does, then hands the opaque owner-encrypted payload to that peer.
//!
//! The hop count is fixed at one by construction. The peer-side handler only
//! ever consults its own attachment table and never re-enters the peer
//! fallback, so a relay request can never loop or fan out across the mesh.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use iroh::{
    Endpoint, EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use protocol::{
    AGENT_CONTROL_RELAY_ALPN, AgentControlOperation, AgentControlRelayError,
    AgentControlRelayIntent, AgentControlRelayRequest, AgentControlRelayResponse,
    IROH_ENDPOINT_ID_BYTES, MAX_AGENT_CONTROL_RELAY_FRAME_BYTES,
};
use tokio::sync::{OnceCell, Semaphore};

use super::{AgentControlForwarder, ForwardError, MemberRegistry};

/// Hard bound on how many peers a single client request may probe. Keeps one
/// unresolvable agent id from turning into an unbounded mesh-wide sweep.
pub const MAX_CONTROL_RELAY_PEER_PROBES: usize = 16;

/// Client half: locates the peer holding an attachment and relays through it.
#[derive(Clone)]
pub struct PeerControlRelay {
    endpoint: Endpoint,
    members: MemberRegistry,
    operation_timeout: Duration,
}

impl PeerControlRelay {
    pub fn new(endpoint: Endpoint, members: MemberRegistry, operation_timeout: Duration) -> Self {
        Self {
            endpoint,
            members,
            operation_timeout,
        }
    }

    /// Delivers `encrypted_payload` to `agent` through whichever peer holds its
    /// attachment. Returns `UnknownAgent` when no peer claims it.
    pub async fn deliver(
        &self,
        agent: EndpointId,
        operation: AgentControlOperation,
        encrypted_payload: Vec<u8>,
    ) -> Result<Vec<u8>, ForwardError> {
        let local = self.endpoint.id();
        let peers: Vec<EndpointId> = self
            .members
            .snapshot()
            .into_iter()
            .filter(|peer| *peer != local)
            .take(MAX_CONTROL_RELAY_PEER_PROBES)
            .collect();
        if peers.is_empty() {
            return Err(ForwardError::UnknownAgent);
        }
        let holder = self.locate(agent, &peers).await.ok_or_else(|| {
            log::debug!(
                "no scheduler peer holds an attachment for agent {}",
                agent.fmt_short()
            );
            ForwardError::UnknownAgent
        })?;
        let request = AgentControlRelayRequest::forward(
            agent.as_bytes().to_vec(),
            operation,
            encrypted_payload,
        );
        match self.exchange(holder, request).await {
            Ok(response) if response.ok => Ok(response.encrypted_payload),
            Ok(response) => Err(map_relay_error(response.error)),
            Err(error) => {
                log::warn!(
                    "relaying {operation:?} for agent {} through scheduler {} failed: {error:#}",
                    agent.fmt_short(),
                    holder.fmt_short()
                );
                Err(ForwardError::Unavailable)
            }
        }
    }

    /// Probes every peer in parallel and returns the first that claims the
    /// attachment. Probing carries no payload, so a wrong guess is cheap.
    async fn locate(&self, agent: EndpointId, peers: &[EndpointId]) -> Option<EndpointId> {
        let probes = peers.iter().copied().map(|peer| async move {
            let request = AgentControlRelayRequest::locate(agent.as_bytes().to_vec());
            match self.exchange(peer, request).await {
                Ok(response) if response.ok => Some(peer),
                Ok(_) => None,
                Err(error) => {
                    log::debug!(
                        "locating agent {} on scheduler {} failed: {error:#}",
                        agent.fmt_short(),
                        peer.fmt_short()
                    );
                    None
                }
            }
        });
        futures::future::join_all(probes)
            .await
            .into_iter()
            .flatten()
            .next()
    }

    async fn exchange(
        &self,
        peer: EndpointId,
        request: AgentControlRelayRequest,
    ) -> Result<AgentControlRelayResponse> {
        let bytes = request.to_bytes()?;
        let connection = tokio::time::timeout(
            self.operation_timeout,
            self.endpoint.connect(peer, AGENT_CONTROL_RELAY_ALPN),
        )
        .await
        .context("scheduler control relay connect timed out")?
        .context("connect scheduler control relay")?;
        ensure!(
            connection.remote_id() == peer,
            "scheduler control relay authenticated an unexpected EndpointId"
        );
        let response = write_then_read(&connection, &bytes, self.operation_timeout).await;
        connection.close(0u8.into(), b"scheduler control relay complete");
        AgentControlRelayResponse::from_bytes(&response?)
    }
}

impl std::fmt::Debug for PeerControlRelay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PeerControlRelay").finish()
    }
}

/// Server half: answers locate probes and delivers relayed payloads to agents
/// attached to this scheduler.
#[derive(Clone)]
pub struct AgentControlRelayHandler {
    forwarder: Arc<OnceCell<AgentControlForwarder>>,
    members: MemberRegistry,
    permits: Arc<Semaphore>,
    operation_timeout: Duration,
}

impl AgentControlRelayHandler {
    pub fn new(
        members: MemberRegistry,
        max_concurrent: usize,
        operation_timeout: Duration,
    ) -> Self {
        Self {
            forwarder: Arc::new(OnceCell::new()),
            members,
            permits: Arc::new(Semaphore::new(max_concurrent)),
            operation_timeout,
        }
    }

    /// The forwarder only exists after the Iroh router is already accepting, so
    /// it is installed once, after startup.
    pub fn install(&self, forwarder: AgentControlForwarder) -> Result<()> {
        self.forwarder
            .set(forwarder)
            .map_err(|_| anyhow::anyhow!("scheduler control relay forwarder was already installed"))
    }

    async fn accept_inner(&self, connection: Connection) -> Result<()> {
        let peer = connection.remote_id();
        ensure!(
            self.members.contains(&peer),
            "scheduler membership required to relay agent control traffic"
        );
        let (mut send, mut recv) =
            tokio::time::timeout(self.operation_timeout, connection.accept_bi())
                .await
                .context("scheduler control relay stream timed out")?
                .context("accept scheduler control relay stream")?;
        let bytes = tokio::time::timeout(
            self.operation_timeout,
            recv.read_to_end(MAX_AGENT_CONTROL_RELAY_FRAME_BYTES),
        )
        .await
        .context("scheduler control relay read timed out")?
        .context("read scheduler control relay request")?;
        let response = self.dispatch(&bytes).await;
        send.write_all(&response.to_bytes()?)
            .await
            .context("write scheduler control relay response")?;
        send.finish()
            .context("finish scheduler control relay response")?;
        let _ = tokio::time::timeout(self.operation_timeout, connection.closed()).await;
        Ok(())
    }

    async fn dispatch(&self, bytes: &[u8]) -> AgentControlRelayResponse {
        let request = match AgentControlRelayRequest::from_bytes(bytes) {
            Ok(request) => request,
            Err(error) => {
                log::warn!("malformed scheduler control relay request: {error:#}");
                return AgentControlRelayResponse::failed(AgentControlRelayError::Rejected);
            }
        };
        let Some(agent) = decode_endpoint_id(&request.agent_endpoint_id) else {
            return AgentControlRelayResponse::failed(AgentControlRelayError::Rejected);
        };
        let Some(forwarder) = self.forwarder.get() else {
            return AgentControlRelayResponse::failed(AgentControlRelayError::Busy);
        };
        match request.intent {
            AgentControlRelayIntent::Locate => {
                if forwarder.holds_attachment(agent).await {
                    AgentControlRelayResponse::located()
                } else {
                    AgentControlRelayResponse::failed(AgentControlRelayError::UnknownAgent)
                }
            }
            AgentControlRelayIntent::Forward(operation) => {
                let Ok(_permit) = self.permits.clone().try_acquire_owned() else {
                    return AgentControlRelayResponse::failed(AgentControlRelayError::Busy);
                };
                // Deliberately the local-only path: a relayed request never
                // triggers another peer hop, so the mesh cannot loop.
                match forwarder
                    .forward_attached(agent, operation, request.encrypted_payload)
                    .await
                {
                    Ok(payload) => AgentControlRelayResponse::delivered(payload),
                    Err(error) => AgentControlRelayResponse::failed(match error {
                        ForwardError::UnknownAgent => AgentControlRelayError::UnknownAgent,
                        ForwardError::Busy => AgentControlRelayError::Busy,
                        ForwardError::Rejected => AgentControlRelayError::Rejected,
                        ForwardError::Unavailable => AgentControlRelayError::Unavailable,
                    }),
                }
            }
        }
    }
}

impl std::fmt::Debug for AgentControlRelayHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AgentControlRelayHandler").finish()
    }
}

impl ProtocolHandler for AgentControlRelayHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.accept_inner(connection)
            .await
            .map_err(|error| AcceptError::from_err(std::io::Error::other(error.to_string())))
    }
}

/// Postcard decodes the id as a byte string; the protocol layer already
/// enforced the length, so a mismatch here means a hostile or broken peer.
fn decode_endpoint_id(bytes: &[u8]) -> Option<EndpointId> {
    let fixed: [u8; IROH_ENDPOINT_ID_BYTES] = bytes.try_into().ok()?;
    EndpointId::from_bytes(&fixed).ok()
}

fn map_relay_error(error: Option<AgentControlRelayError>) -> ForwardError {
    match error {
        Some(AgentControlRelayError::UnknownAgent) | None => ForwardError::UnknownAgent,
        Some(AgentControlRelayError::Busy) => ForwardError::Busy,
        Some(AgentControlRelayError::Rejected) => ForwardError::Rejected,
        Some(AgentControlRelayError::Unavailable) => ForwardError::Unavailable,
    }
}

async fn write_then_read(
    connection: &Connection,
    bytes: &[u8],
    operation_timeout: Duration,
) -> Result<Vec<u8>> {
    let (mut send, mut recv) = tokio::time::timeout(operation_timeout, connection.open_bi())
        .await
        .context("scheduler control relay stream timed out")?
        .context("open scheduler control relay stream")?;
    send.write_all(bytes)
        .await
        .context("write scheduler control relay request")?;
    send.finish()
        .context("finish scheduler control relay request")?;
    tokio::time::timeout(
        operation_timeout,
        recv.read_to_end(MAX_AGENT_CONTROL_RELAY_FRAME_BYTES),
    )
    .await
    .context("scheduler control relay response timed out")?
    .context("read scheduler control relay response")
}
