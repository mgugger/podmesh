use std::time::Duration;

use anyhow::{Context, Result};
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use protocol::{
    AdmissionRequest, AgentControlOperation, AgentControlRequest, AgentControlResponse,
    DeploymentGrant, MAX_AGENT_CONTROL_FRAME_BYTES, UpdateRequest, WorkloadCommand,
};

use crate::AgentService;

#[derive(Clone)]
pub struct AgentControlHandler {
    service: AgentService,
    operation_timeout: Duration,
    dispatch_timeout: Duration,
}

impl AgentControlHandler {
    pub fn new(service: AgentService, operation_timeout: Duration) -> Self {
        let dispatch_timeout = service.control_dispatch_timeout();
        Self {
            service,
            operation_timeout,
            dispatch_timeout,
        }
    }

    async fn accept_inner(&self, connection: Connection) -> Result<()> {
        let (mut send, mut recv) =
            tokio::time::timeout(self.operation_timeout, connection.accept_bi())
                .await
                .context("agent control stream timed out")?
                .context("accept agent control stream")?;
        let bytes = tokio::time::timeout(
            self.operation_timeout,
            recv.read_to_end(MAX_AGENT_CONTROL_FRAME_BYTES),
        )
        .await
        .context("agent control request read timed out")?
        .context("read agent control request")?;
        let response = match tokio::time::timeout(self.dispatch_timeout, self.dispatch(&bytes))
            .await
            .context("agent control dispatch timed out")?
        {
            Ok(encrypted_payload) => AgentControlResponse::success(encrypted_payload),
            Err(error) => {
                log::warn!(
                    "agent control request from {} rejected: {error}",
                    connection.remote_id().fmt_short()
                );
                AgentControlResponse::rejected()
            }
        };
        tokio::time::timeout(
            self.operation_timeout,
            send.write_all(&response.to_bytes()?),
        )
        .await
        .context("agent control response write timed out")?
        .context("write agent control response")?;
        send.finish().context("finish agent control response")?;
        let _ = tokio::time::timeout(self.operation_timeout, connection.closed()).await;
        Ok(())
    }

    async fn dispatch(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        let request = AgentControlRequest::from_bytes(bytes)?;
        match request.operation {
            AgentControlOperation::Admission => {
                let admission = self
                    .service
                    .decrypt::<AdmissionRequest>(&request.encrypted_payload)?;
                self.service.admit(admission).await
            }
            AgentControlOperation::Deploy => {
                let grant = self
                    .service
                    .decrypt::<DeploymentGrant>(&request.encrypted_payload)?;
                self.service.deploy(grant).await
            }
            AgentControlOperation::Update => {
                let request = self
                    .service
                    .decrypt::<UpdateRequest>(&request.encrypted_payload)?;
                self.service.update(request).await
            }
            AgentControlOperation::Command => {
                let command = self
                    .service
                    .decrypt::<WorkloadCommand>(&request.encrypted_payload)?;
                self.service.command(command).await
            }
            AgentControlOperation::List => {
                // A list request is not sealed: it carries nothing secret and
                // must be broadcastable to agents the client cannot name.
                let list = protocol::WorkloadListRequest::from_bytes(
                    &request.encrypted_payload,
                    crate::service::now_secs(),
                )?;
                self.service.list(list).await
            }
        }
    }
}

impl std::fmt::Debug for AgentControlHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AgentControlHandler").finish()
    }
}

impl ProtocolHandler for AgentControlHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.accept_inner(connection)
            .await
            .map_err(|error| AcceptError::from_err(std::io::Error::other(error.to_string())))
    }
}
