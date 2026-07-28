use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use futures::{StreamExt, stream};
use iroh::{
    EndpointAddr, EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use protocol::{
    AgentAttachmentAck, AgentAttachmentHello, CapacityQuery, MAX_AGENT_ATTACHMENT_BYTES,
    MachineRole, SCHEDULER_MESH_PROTOCOL_VERSION,
};
use tokio::sync::Mutex;
use uuid::Uuid;

const MAX_CONCURRENT_FANOUT: usize = 32;

#[derive(Clone)]
pub struct AttachmentManager {
    inner: Arc<Mutex<HashMap<EndpointId, AttachmentSession>>>,
    max_attached_agents: usize,
    max_agent_fanout: usize,
    operation_timeout: Duration,
    relay_issuer: Option<RelayGrantIssuer>,
}

#[derive(Clone)]
struct RelayGrantIssuer {
    identity: super::SchedulerIdentity,
    audience: String,
}

#[derive(Clone)]
struct AttachmentSession {
    generation: Uuid,
    connection: Connection,
    /// Address hints taken from the agent's signed attachment hello. Used only
    /// to dial the agent back on `AGENT_CONTROL_ALPN`; the authenticated remote
    /// EndpointId is what actually authorizes that connection.
    agent_addr: EndpointAddr,
}

impl AttachmentManager {
    pub fn new(
        max_attached_agents: usize,
        max_agent_fanout: usize,
        operation_timeout: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::with_capacity(max_attached_agents))),
            max_attached_agents,
            max_agent_fanout,
            operation_timeout,
            relay_issuer: None,
        }
    }

    pub fn with_relay_grant_issuer(
        mut self,
        identity: super::SchedulerIdentity,
        audience: String,
    ) -> Self {
        self.relay_issuer = Some(RelayGrantIssuer { identity, audience });
        self
    }

    pub fn handler(&self) -> AgentAttachmentHandler {
        AgentAttachmentHandler {
            manager: self.clone(),
        }
    }

    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// Dialable address of an attached agent, if it currently holds a session.
    pub async fn agent_addr(&self, endpoint_id: EndpointId) -> Option<EndpointAddr> {
        self.inner
            .lock()
            .await
            .get(&endpoint_id)
            .map(|session| session.agent_addr.clone())
    }

    pub async fn fanout(&self, query: &CapacityQuery) -> Result<usize> {
        let bytes = query.to_bytes(now_secs())?;
        let mut sessions: Vec<_> = self
            .inner
            .lock()
            .await
            .iter()
            .map(|(endpoint_id, session)| (*endpoint_id, session.connection.clone()))
            .collect();
        sessions.sort_unstable_by_key(|(endpoint_id, _)| *endpoint_id);
        sessions.truncate(self.max_agent_fanout);
        let timeout = self.operation_timeout;
        let successes = stream::iter(sessions)
            .map(|(endpoint_id, connection)| {
                let bytes = bytes.clone();
                async move {
                    let result = tokio::time::timeout(timeout, async {
                        let mut send = connection
                            .open_uni()
                            .await
                            .context("open capacity query stream")?;
                        send.write_all(&bytes)
                            .await
                            .context("write capacity query")?;
                        send.finish().context("finish capacity query stream")?;
                        Result::<()>::Ok(())
                    })
                    .await
                    .context("capacity query fanout timed out")?;
                    result.with_context(|| {
                        format!(
                            "fan out capacity query to agent {}",
                            endpoint_id.fmt_short()
                        )
                    })
                }
            })
            .buffer_unordered(MAX_CONCURRENT_FANOUT)
            .filter_map(|result| async move {
                match result {
                    Ok(()) => Some(()),
                    Err(error) => {
                        log::warn!("agent capacity fanout failed: {error}");
                        None
                    }
                }
            })
            .count()
            .await;
        Ok(successes)
    }

    async fn register(
        &self,
        endpoint_id: EndpointId,
        connection: Connection,
        agent_addr: EndpointAddr,
    ) -> Result<Uuid> {
        let mut sessions = self.inner.lock().await;
        ensure!(
            !sessions.contains_key(&endpoint_id),
            "agent already has an active scheduler attachment"
        );
        ensure!(
            sessions.len() < self.max_attached_agents,
            "scheduler attachment limit reached"
        );
        let generation = Uuid::new_v4();
        sessions.insert(
            endpoint_id,
            AttachmentSession {
                generation,
                connection,
                agent_addr,
            },
        );
        Ok(generation)
    }

    async fn remove(&self, endpoint_id: EndpointId, generation: Uuid) {
        let mut sessions = self.inner.lock().await;
        if sessions
            .get(&endpoint_id)
            .is_some_and(|session| session.generation == generation)
        {
            sessions.remove(&endpoint_id);
        }
    }

    fn acknowledgement(
        &self,
        endpoint_id: EndpointId,
        now_secs: u64,
    ) -> Result<AgentAttachmentAck> {
        let relay_grants = self
            .relay_issuer
            .as_ref()
            .map(|issuer| {
                issuer
                    .identity
                    .issue_relay_grant(
                        endpoint_id,
                        MachineRole::Agent,
                        issuer.audience.clone(),
                        now_secs,
                    )?
                    .to_auth_token(now_secs)
            })
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        Ok(AgentAttachmentAck {
            version: SCHEDULER_MESH_PROTOCOL_VERSION,
            relay_grants,
            refresh_after_secs: now_secs + protocol::MAX_MACHINE_RELAY_GRANT_LIFETIME_SECS / 2,
        })
    }
}

#[derive(Clone)]
pub struct AgentAttachmentHandler {
    manager: AttachmentManager,
}

impl std::fmt::Debug for AgentAttachmentHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AgentAttachmentHandler").finish()
    }
}

impl ProtocolHandler for AgentAttachmentHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.accept_inner(connection)
            .await
            .map_err(|error| AcceptError::from_err(std::io::Error::other(error.to_string())))
    }
}

impl AgentAttachmentHandler {
    async fn accept_inner(&self, connection: Connection) -> Result<()> {
        let remote_id = connection.remote_id();
        let (mut send, mut recv) =
            tokio::time::timeout(self.manager.operation_timeout, connection.accept_bi())
                .await
                .context("agent attachment handshake timed out")?
                .context("accept agent attachment handshake stream")?;
        let bytes = tokio::time::timeout(
            self.manager.operation_timeout,
            recv.read_to_end(MAX_AGENT_ATTACHMENT_BYTES),
        )
        .await
        .context("agent attachment hello read timed out")?
        .context("read agent attachment hello")?;
        let hello = AgentAttachmentHello::from_bytes(&bytes, now_secs())?;
        ensure!(
            hello.agent_endpoint.endpoint_id == remote_id.as_bytes(),
            "agent attachment EndpointId does not match authenticated transport"
        );
        let agent_addr = iroh_support::endpoint_addr(&hello.agent_endpoint, now_secs())?;
        let generation = self
            .manager
            .register(remote_id, connection.clone(), agent_addr)
            .await?;
        let acknowledgement = self.manager.acknowledgement(remote_id, now_secs())?;
        send.write_all(&acknowledgement.to_bytes(now_secs())?)
            .await
            .context("write agent attachment acknowledgement")?;
        send.finish()
            .context("finish agent attachment acknowledgement")?;
        log::info!("agent attached to scheduler: {}", remote_id.fmt_short());
        let _ = connection.closed().await;
        self.manager.remove(remote_id, generation).await;
        log::info!("agent detached from scheduler: {}", remote_id.fmt_short());
        Ok(())
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
