//! One-hop scheduler-to-scheduler relay of owner control traffic.
//!
//! An agent holds exactly one scheduler attachment, but `podctl` is expected to
//! be able to talk to any scheduler it can reach. This module closes that gap:
//! a scheduler that does not hold the target attachment asks the mesh which one
//! does, then hands the opaque owner-encrypted payload to that peer.
//!
//! The hop count is fixed at one by construction. The peer-side handler, in
//! [`super::relay_handler`], only ever consults its own attachment table and
//! never re-enters this fallback, so a relay request can never loop or fan out
//! across the mesh.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, EndpointId, endpoint::Connection};
use protocol::{
    AGENT_CONTROL_RELAY_ALPN, AgentControlOperation, AgentControlRelayError,
    AgentControlRelayRequest, AgentControlRelayResponse, MAX_AGENT_CONTROL_RELAY_FRAME_BYTES,
};
use tokio::sync::OnceCell;

use super::{ForwardError, MemberRegistry};
use crate::now_secs;

/// How long to wait for a gossiped location query to be answered.
///
/// The holder answers with one direct connection, so this is a network round
/// trip plus gossip propagation, not a sweep. A query that goes unanswered means
/// no scheduler in the mesh holds the agent.
const LOCATION_ANSWER_TIMEOUT: Duration = Duration::from_secs(3);

/// Client half: locates the peer holding an attachment and relays through it.
#[derive(Clone)]
pub struct PeerControlRelay {
    endpoint: Endpoint,
    members: MemberRegistry,
    locations: super::LocationRegistry,
    locator: Arc<OnceCell<super::GossipPublisher>>,
    identity: super::SchedulerIdentity,
    operation_timeout: Duration,
}

impl PeerControlRelay {
    pub fn new(
        endpoint: Endpoint,
        members: MemberRegistry,
        locations: super::LocationRegistry,
        identity: super::SchedulerIdentity,
        operation_timeout: Duration,
    ) -> Self {
        Self {
            endpoint,
            members,
            locations,
            locator: Arc::new(OnceCell::new()),
            identity,
            operation_timeout,
        }
    }

    /// Gossip only exists after the router is accepting, so the publisher is
    /// installed once startup finishes.
    pub fn install_publisher(&self, publisher: super::GossipPublisher) -> Result<()> {
        self.locator
            .set(publisher)
            .map_err(|_| anyhow::anyhow!("scheduler location publisher was already installed"))
    }

    /// Delivers `encrypted_payload` to `agent` through whichever peer holds its
    /// attachment. Returns `UnknownAgent` when no peer claims it.
    pub async fn deliver(
        &self,
        agent: EndpointId,
        operation: AgentControlOperation,
        encrypted_payload: Vec<u8>,
    ) -> Result<Vec<u8>, ForwardError> {
        // A cached location is tried first and, if it turns out to be stale,
        // forgotten so the next attempt resolves again rather than failing the
        // same way forever.
        if let Some(holder) = self.locations.cached(agent).await
            && let Some(response) = self
                .try_forward(holder, agent, operation, encrypted_payload.clone())
                .await
        {
            return response;
        }

        let holder = self.locate(agent).await.ok_or_else(|| {
            log::debug!(
                "no scheduler holds an attachment for agent {}",
                agent.fmt_short()
            );
            ForwardError::UnknownAgent
        })?;
        self.try_forward(holder, agent, operation, encrypted_payload)
            .await
            .unwrap_or(Err(ForwardError::Unavailable))
    }

    /// Relay through one peer. `None` means that peer could not be used, so the
    /// caller should resolve the location again.
    async fn try_forward(
        &self,
        holder: EndpointId,
        agent: EndpointId,
        operation: AgentControlOperation,
        encrypted_payload: Vec<u8>,
    ) -> Option<Result<Vec<u8>, ForwardError>> {
        let request = AgentControlRelayRequest::forward(
            agent.as_bytes().to_vec(),
            operation,
            encrypted_payload,
        );
        // Locations are learned from gossip, so a holder that has since been
        // evicted from membership must not receive owner traffic.
        if !self.members.contains(&holder) {
            self.locations.forget(agent).await;
            return None;
        }
        match self.exchange(holder, request).await {
            Ok(response) if response.ok => Some(Ok(response.encrypted_payload)),
            // The peer answered that it does not hold the agent, so whatever we
            // believed about its location is wrong.
            Ok(response) if response.error == Some(AgentControlRelayError::UnknownAgent) => {
                self.locations.forget(agent).await;
                None
            }
            Ok(response) => Some(Err(map_relay_error(response.error))),
            Err(error) => {
                log::warn!(
                    "relaying {operation:?} for agent {} through scheduler {} failed: {error:#}",
                    agent.fmt_short(),
                    holder.fmt_short()
                );
                self.locations.forget(agent).await;
                None
            }
        }
    }

    /// Ask the mesh which scheduler holds `agent`.
    ///
    /// One gossiped query reaches every scheduler and only the holder answers,
    /// so the cost is independent of how many schedulers there are. Probing each
    /// peer instead would cost a QUIC connection per peer, per request.
    async fn locate(&self, agent: EndpointId) -> Option<EndpointId> {
        match self.query_mesh(agent).await {
            Ok(holder) => holder,
            Err(error) => {
                // Silently returning "unknown agent" here would make a local
                // misconfiguration look exactly like an agent that is not in
                // the mesh, so the reason is always reported.
                log::warn!(
                    "locating agent {} over scheduler gossip failed: {error:#}",
                    agent.fmt_short()
                );
                None
            }
        }
    }

    async fn query_mesh(&self, agent: EndpointId) -> Result<Option<EndpointId>> {
        let publisher = self
            .locator
            .get()
            .context("scheduler gossip publisher is not installed")?;
        let now = now_secs();
        let query_id = uuid::Uuid::new_v4().to_string();
        let reply_endpoint = self.identity.endpoint_record(
            &self.endpoint.addr(),
            now,
            now + protocol::MAX_LOCATION_QUERY_LIFETIME_SECS,
        )?;
        let query = protocol::AgentLocationQuery {
            version: protocol::SCHEDULER_GOSSIP_PROTOCOL_VERSION,
            query_id: query_id.clone(),
            agent_endpoint_id: agent.as_bytes().to_vec(),
            reply_endpoint,
            issued_at_secs: now,
            expires_at_secs: now + protocol::MAX_LOCATION_QUERY_LIFETIME_SECS,
            signing_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(
            self.identity.signing_public(),
            self.identity.signing_private(),
            now,
        )?;

        let mut answer = self.locations.begin(&query_id).await?;
        let result = async {
            publisher.publish_location(query).await?;
            // No answer means no scheduler in the mesh holds the agent, which
            // is a legitimate outcome rather than a failure.
            Ok(tokio::time::timeout(
                LOCATION_ANSWER_TIMEOUT,
                answer.wait_for(|holder| holder.is_some()),
            )
            .await
            .ok()
            .and_then(|holder| holder.ok().and_then(|holder| *holder)))
        }
        .await;
        self.locations.finish(&query_id).await;
        result
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
