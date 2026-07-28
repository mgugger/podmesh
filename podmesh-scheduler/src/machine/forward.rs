//! Relays owner control traffic from the HTTP client API to a selected agent.
//!
//! `podctl` is a plain CLI and never joins the Iroh mesh, so the scheduler is
//! the only component that can reach an agent's control endpoint. Every payload
//! forwarded here is already signed by the namespace owner and encrypted to the
//! agent's KEM key: the scheduler moves opaque bytes and can neither read nor
//! forge them.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, EndpointId};
use protocol::{
    AGENT_CONTROL_ALPN, AGENT_CONTROL_PROTOCOL_VERSION, AgentControlOperation, AgentControlRequest,
    AgentControlResponse, MAX_AGENT_CONTROL_FRAME_BYTES,
};
use tokio::sync::Semaphore;

use super::AttachmentManager;

#[derive(Clone)]
pub struct AgentControlForwarder {
    endpoint: Endpoint,
    attachments: AttachmentManager,
    operation_timeout: Duration,
    permits: std::sync::Arc<Semaphore>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardError {
    /// No attached agent holds this EndpointId.
    UnknownAgent,
    /// The scheduler is already relaying its configured maximum.
    Busy,
    /// The agent refused the owner-signed payload.
    Rejected,
    /// The agent could not be reached or answered incoherently.
    Unavailable,
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnknownAgent => "agent is not attached to this scheduler",
            Self::Busy => "scheduler control relay is saturated",
            Self::Rejected => "agent rejected the request",
            Self::Unavailable => "agent control endpoint is unreachable",
        };
        formatter.write_str(message)
    }
}

impl AgentControlForwarder {
    pub fn new(
        endpoint: Endpoint,
        attachments: AttachmentManager,
        operation_timeout: Duration,
        max_concurrent: usize,
    ) -> Self {
        Self {
            endpoint,
            attachments,
            operation_timeout,
            permits: std::sync::Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    pub async fn forward(
        &self,
        agent: EndpointId,
        operation: AgentControlOperation,
        encrypted_payload: Vec<u8>,
    ) -> Result<Vec<u8>, ForwardError> {
        let _permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| ForwardError::Busy)?;
        let address = self
            .attachments
            .agent_addr(agent)
            .await
            .ok_or(ForwardError::UnknownAgent)?;
        let request = AgentControlRequest {
            version: AGENT_CONTROL_PROTOCOL_VERSION,
            operation,
            encrypted_payload,
        };
        match self.exchange(agent, address, request).await {
            Ok(response) if response.ok => Ok(response.encrypted_payload),
            Ok(_) => Err(ForwardError::Rejected),
            Err(error) => {
                log::warn!(
                    "forwarding {operation:?} to agent {} failed: {error}",
                    agent.fmt_short()
                );
                Err(ForwardError::Unavailable)
            }
        }
    }

    async fn exchange(
        &self,
        agent: EndpointId,
        address: iroh::EndpointAddr,
        request: AgentControlRequest,
    ) -> Result<AgentControlResponse> {
        let bytes = request.to_bytes()?;
        let connection = tokio::time::timeout(
            self.operation_timeout,
            self.endpoint.connect(address, AGENT_CONTROL_ALPN),
        )
        .await
        .context("agent control connect timed out")?
        .context("connect agent control endpoint")?;
        ensure!(
            connection.remote_id() == agent,
            "agent control connection authenticated an unexpected EndpointId"
        );
        let (mut send, mut recv) =
            tokio::time::timeout(self.operation_timeout, connection.open_bi())
                .await
                .context("agent control stream timed out")?
                .context("open agent control stream")?;
        send.write_all(&bytes)
            .await
            .context("write agent control request")?;
        send.finish().context("finish agent control request")?;
        let response_bytes = tokio::time::timeout(
            self.operation_timeout,
            recv.read_to_end(MAX_AGENT_CONTROL_FRAME_BYTES),
        )
        .await
        .context("agent control response timed out")?
        .context("read agent control response")?;
        let response = AgentControlResponse::from_bytes(&response_bytes)?;
        connection.close(0u8.into(), b"agent control complete");
        Ok(response)
    }
}

impl std::fmt::Debug for AgentControlForwarder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AgentControlForwarder").finish()
    }
}
