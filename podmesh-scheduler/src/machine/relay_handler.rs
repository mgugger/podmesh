//! Server half of the scheduler control relay.
//!
//! A scheduler answers here on behalf of the agents attached to it: it delivers
//! relayed owner payloads to those agents, and records the answers to the
//! location queries it asked. It never re-enters the peer fallback, so a relayed
//! request can neither loop nor fan out across the mesh.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use iroh::{
    EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use protocol::{
    AgentControlRelayError, AgentControlRelayIntent, AgentControlRelayRequest,
    AgentControlRelayResponse, IROH_ENDPOINT_ID_BYTES, MAX_AGENT_CONTROL_RELAY_FRAME_BYTES,
};
use tokio::sync::{OnceCell, Semaphore};

use super::{AgentControlForwarder, ForwardError, MemberRegistry};

/// Server half: answers locate probes and delivers relayed payloads to agents
/// attached to this scheduler.
#[derive(Clone)]
pub struct AgentControlRelayHandler {
    forwarder: Arc<OnceCell<AgentControlForwarder>>,
    members: MemberRegistry,
    locations: super::LocationRegistry,
    permits: Arc<Semaphore>,
    operation_timeout: Duration,
}

impl AgentControlRelayHandler {
    pub fn new(
        members: MemberRegistry,
        locations: super::LocationRegistry,
        max_concurrent: usize,
        operation_timeout: Duration,
    ) -> Self {
        Self {
            forwarder: Arc::new(OnceCell::new()),
            members,
            locations,
            permits: Arc::new(Semaphore::new(max_concurrent)),
            operation_timeout,
        }
    }

    /// Shared handle to the forwarder, for tasks that must consult attachments
    /// once startup has completed.
    pub fn forwarder_handle(&self) -> Arc<OnceCell<AgentControlForwarder>> {
        self.forwarder.clone()
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
        let response = self.dispatch(&bytes, peer).await;
        send.write_all(&response.to_bytes()?)
            .await
            .context("write scheduler control relay response")?;
        send.finish()
            .context("finish scheduler control relay response")?;
        let _ = tokio::time::timeout(self.operation_timeout, connection.closed()).await;
        Ok(())
    }

    async fn dispatch(&self, bytes: &[u8], peer: EndpointId) -> AgentControlRelayResponse {
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
            AgentControlRelayIntent::LocationAnswer => {
                // The holder is the authenticated peer, never anything it
                // claims, so a member cannot plant a location for somebody else.
                self.locations.resolve(&request.query_id, agent, peer).await;
                AgentControlRelayResponse::located()
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
