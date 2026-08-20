use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use iroh::Endpoint;
use tokio::sync::OnceCell;

use super::{
    AgentControlForwarder, MAX_RECONCILIATION_AGENTS, ReconciliationRegistry, SchedulerIdentity,
};

const MAX_CONCURRENT_RECONCILIATION_PROBES: usize = 32;

#[derive(Clone)]
pub struct ReconciliationResponder {
    identity: SchedulerIdentity,
    endpoint: Endpoint,
    registry: ReconciliationRegistry,
    forwarder: Arc<OnceCell<AgentControlForwarder>>,
    timeout: Duration,
}

impl ReconciliationResponder {
    pub fn new(
        identity: SchedulerIdentity,
        endpoint: Endpoint,
        registry: ReconciliationRegistry,
        forwarder: Arc<OnceCell<AgentControlForwarder>>,
        timeout: Duration,
    ) -> Self {
        Self {
            identity,
            endpoint,
            registry,
            forwarder,
            timeout,
        }
    }

    pub async fn answer_local(&self, query: protocol::SchedulerReconciliationQuery) -> Result<()> {
        let events = self.collect(&query).await?;
        for event in events {
            let response = self.signed_response(&query.query_id, event)?;
            self.registry.record(self.endpoint.id(), response).await?;
        }
        Ok(())
    }

    pub async fn answer_remote(&self, query: protocol::SchedulerReconciliationQuery) -> Result<()> {
        let events = self.collect(&query).await?;
        let address = iroh_support::endpoint_addr(&query.reply_endpoint, crate::now_secs())?;
        let connection = tokio::time::timeout(
            self.timeout,
            self.endpoint
                .connect(address, protocol::SCHEDULER_RECONCILIATION_ALPN),
        )
        .await
        .context("reconciliation response connect timed out")?
        .context("connect reconciliation response endpoint")?;
        for event in events {
            let response = self.signed_response(&query.query_id, event)?;
            let bytes = response.to_bytes(crate::now_secs())?;
            let (mut send, mut recv) = tokio::time::timeout(self.timeout, connection.open_bi())
                .await
                .context("open reconciliation response stream timed out")?
                .context("open reconciliation response stream")?;
            send.write_all(&bytes)
                .await
                .context("write reconciliation response")?;
            send.finish().context("finish reconciliation response")?;
            let ack = tokio::time::timeout(self.timeout, recv.read_to_end(1))
                .await
                .context("reconciliation response acknowledgement timed out")?
                .context("read reconciliation response acknowledgement")?;
            ensure!(
                ack == [1],
                "invalid reconciliation response acknowledgement"
            );
        }
        connection.close(0u8.into(), b"reconciliation complete");
        Ok(())
    }

    async fn collect(
        &self,
        query: &protocol::SchedulerReconciliationQuery,
    ) -> Result<Vec<protocol::ReconciliationEvent>> {
        use futures::StreamExt;

        let forwarder = self
            .forwarder
            .get()
            .context("agent control forwarder is not installed")?;
        let attached = forwarder.attached_agents().await;
        ensure!(
            attached.len() <= MAX_RECONCILIATION_AGENTS,
            "local agent count exceeds reconciliation fanout bound"
        );
        let mut events = futures::stream::iter(attached.into_iter().map(|agent| {
            let payload = query.owner_request.clone();
            async move {
                match forwarder
                    .forward_attached_wait(agent, protocol::AgentControlOperation::List, payload)
                    .await
                {
                    Ok(sealed_response) => protocol::ReconciliationEvent::AgentAnswer {
                        agent_endpoint_id: agent.as_bytes().to_vec(),
                        sealed_response,
                    },
                    Err(_) => protocol::ReconciliationEvent::AgentUnreachable {
                        agent_endpoint_id: agent.as_bytes().to_vec(),
                    },
                }
            }
        }))
        .buffer_unordered(MAX_CONCURRENT_RECONCILIATION_PROBES)
        .collect::<Vec<_>>()
        .await;
        events.push(protocol::ReconciliationEvent::Complete);
        Ok(events)
    }

    fn signed_response(
        &self,
        query_id: &str,
        event: protocol::ReconciliationEvent,
    ) -> Result<protocol::SchedulerReconciliationResponse> {
        let now = crate::now_secs();
        let responder_endpoint = self.identity.endpoint_record(
            &self.endpoint.addr(),
            now,
            now + protocol::MAX_RECONCILIATION_LIFETIME_SECS,
        )?;
        protocol::SchedulerReconciliationResponse {
            version: protocol::RECONCILIATION_PROTOCOL_VERSION,
            query_id: query_id.to_string(),
            responder_endpoint,
            event,
            responded_at_secs: now,
            signing_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(
            self.identity.signing_public(),
            self.identity.signing_private(),
            now,
        )
    }
}
