//! A scheduler that holds no attachment must still deliver control traffic.
//!
//! `podctl` picks whichever scheduler it can reach, but an agent attaches to
//! exactly one. Without a peer hop the client would be told the agent does not
//! exist by every scheduler except the one holding the attachment.

mod common;

use std::{collections::HashSet, net::SocketAddr, time::Duration};

use anyhow::{Context, Result, ensure};
use common::{TEST_TIMEOUT, config, endpoint, now_secs};
use iroh::{
    Endpoint, EndpointId, SecretKey,
    address_lookup::memory::MemoryLookup,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
};
use podmesh_scheduler::machine::{
    AgentControlForwarder, AttachmentManager, ForwardError, PeerControlRelay, PlacementHandler,
    QueryManager, SchedulerGossip,
};
use protocol::{
    AGENT_CAPACITY_ALPN, AGENT_CONTROL_ALPN, AgentAttachmentHello, AgentControlOperation,
    AgentControlRequest, AgentControlResponse, ENDPOINT_RECORD_VERSION, EndpointRecord,
    MAX_AGENT_CONTROL_FRAME_BYTES, SCHEDULER_MESH_PROTOCOL_VERSION,
};
use tokio::time::timeout;

const MAX_CONCURRENT_RELAYS: usize = 4;
const ATTACHMENT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Fake agent control endpoint: it never decrypts anything, it only proves the
/// owner payload arrived unchanged and that its answer travels back.
#[derive(Debug, Clone)]
struct EchoAgentControl;

impl ProtocolHandler for EchoAgentControl {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection
            .accept_bi()
            .await
            .map_err(|error| AcceptError::from_err(std::io::Error::other(error.to_string())))?;
        let bytes = recv
            .read_to_end(MAX_AGENT_CONTROL_FRAME_BYTES)
            .await
            .map_err(|error| AcceptError::from_err(std::io::Error::other(error.to_string())))?;
        let request = AgentControlRequest::from_bytes(&bytes)
            .map_err(|error| AcceptError::from_err(std::io::Error::other(error.to_string())))?;
        let mut echoed = request.encrypted_payload;
        echoed.reverse();
        let response = AgentControlResponse::success(echoed)
            .to_bytes()
            .map_err(|error| AcceptError::from_err(std::io::Error::other(error.to_string())))?;
        send.write_all(&response)
            .await
            .map_err(|error| AcceptError::from_err(std::io::Error::other(error.to_string())))?;
        send.finish()
            .map_err(|error| AcceptError::from_err(std::io::Error::other(error.to_string())))?;
        connection.closed().await;
        Ok(())
    }
}

struct TestScheduler {
    endpoint: Endpoint,
    gossip: SchedulerGossip,
    attachments: AttachmentManager,
    forwarder: AgentControlForwarder,
}

impl TestScheduler {
    async fn start(endpoint: Endpoint, members: HashSet<EndpointId>) -> Result<Self> {
        let machine_config = config(members, Vec::new());
        let attachments = AttachmentManager::new(4, 4, TEST_TIMEOUT);
        let queries = QueryManager::new(4, 4, TEST_TIMEOUT);
        let gossip = SchedulerGossip::start(
            endpoint.clone(),
            &machine_config,
            attachments.handler(),
            queries.offer_handler(),
            PlacementHandler::new(4, TEST_TIMEOUT),
        )
        .await?;
        let forwarder = AgentControlForwarder::new(
            endpoint.clone(),
            attachments.clone(),
            PeerControlRelay::new(endpoint.clone(), gossip.members(), TEST_TIMEOUT),
            TEST_TIMEOUT,
            MAX_CONCURRENT_RELAYS,
        );
        gossip.control_relay().install(forwarder.clone())?;
        Ok(Self {
            endpoint,
            gossip,
            attachments,
            forwarder,
        })
    }

    async fn shutdown(self) -> Result<()> {
        timeout(TEST_TIMEOUT, self.gossip.shutdown()).await??;
        timeout(TEST_TIMEOUT, self.endpoint.close()).await?;
        Ok(())
    }
}

#[tokio::test]
async fn a_scheduler_without_the_attachment_relays_through_the_peer_that_has_it() -> Result<()> {
    let lookup = MemoryLookup::new();
    let entry = endpoint(&SecretKey::generate(), &lookup).await?;
    let holder = endpoint(&SecretKey::generate(), &lookup).await?;
    let agent = endpoint(&SecretKey::generate(), &lookup).await?;
    for known in [&entry, &holder, &agent] {
        lookup.add_endpoint_info(known.addr());
    }
    let members = HashSet::from([entry.id(), holder.id()]);
    let entry_scheduler = TestScheduler::start(entry.clone(), members.clone()).await?;
    let holder_scheduler = TestScheduler::start(holder.clone(), members).await?;

    let agent_router = Router::builder(agent.clone())
        .accept(AGENT_CONTROL_ALPN, EchoAgentControl)
        .spawn();
    let attachment = attach(&agent, &holder).await?;
    wait_for_attachment(&holder_scheduler.attachments, agent.id()).await?;
    ensure!(
        entry_scheduler
            .attachments
            .agent_addr(agent.id())
            .await
            .is_none(),
        "entry scheduler must not hold the agent attachment"
    );

    let payload = vec![11u8, 22, 33, 44];
    let mut expected = payload.clone();
    expected.reverse();
    let relayed = timeout(
        TEST_TIMEOUT,
        entry_scheduler.forwarder.forward(
            agent.id(),
            AgentControlOperation::Admission,
            payload.clone(),
        ),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("peer relay failed: {error}"))?;
    ensure!(
        relayed == expected,
        "relayed payload was not delivered intact"
    );

    // An agent nobody holds must fail closed rather than hang or succeed.
    let unknown = SecretKey::generate().public();
    let refused = timeout(
        TEST_TIMEOUT,
        entry_scheduler
            .forwarder
            .forward(unknown, AgentControlOperation::Command, payload),
    )
    .await?;
    ensure!(
        refused == Err(ForwardError::UnknownAgent),
        "an agent no scheduler holds must be reported as unknown"
    );

    attachment.close(0u8.into(), b"test complete");
    timeout(TEST_TIMEOUT, agent_router.shutdown()).await??;
    timeout(TEST_TIMEOUT, agent.close()).await?;
    entry_scheduler.shutdown().await?;
    holder_scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn a_relayed_request_from_a_non_member_scheduler_is_refused() -> Result<()> {
    let lookup = MemoryLookup::new();
    let holder = endpoint(&SecretKey::generate(), &lookup).await?;
    let stranger = endpoint(&SecretKey::generate(), &lookup).await?;
    let agent = endpoint(&SecretKey::generate(), &lookup).await?;
    for known in [&holder, &stranger, &agent] {
        lookup.add_endpoint_info(known.addr());
    }
    // The stranger is deliberately absent from the holder's member allowlist.
    let holder_scheduler =
        TestScheduler::start(holder.clone(), HashSet::from([holder.id()])).await?;
    let stranger_scheduler = TestScheduler::start(
        stranger.clone(),
        HashSet::from([stranger.id(), holder.id()]),
    )
    .await?;

    let agent_router = Router::builder(agent.clone())
        .accept(AGENT_CONTROL_ALPN, EchoAgentControl)
        .spawn();
    let attachment = attach(&agent, &holder).await?;
    wait_for_attachment(&holder_scheduler.attachments, agent.id()).await?;

    let refused = timeout(
        TEST_TIMEOUT,
        stranger_scheduler.forwarder.forward(
            agent.id(),
            AgentControlOperation::Deploy,
            vec![7u8; 16],
        ),
    )
    .await?;
    ensure!(
        refused == Err(ForwardError::UnknownAgent),
        "a scheduler outside the member allowlist must not be able to relay"
    );

    attachment.close(0u8.into(), b"test complete");
    timeout(TEST_TIMEOUT, agent_router.shutdown()).await??;
    timeout(TEST_TIMEOUT, agent.close()).await?;
    stranger_scheduler.shutdown().await?;
    holder_scheduler.shutdown().await?;
    Ok(())
}

async fn attach(agent: &Endpoint, scheduler: &Endpoint) -> Result<Connection> {
    let connection = agent
        .connect(scheduler.addr(), AGENT_CAPACITY_ALPN)
        .await
        .context("open agent attachment")?;
    let now = now_secs();
    let hello = signed_hello(agent, now)?;
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&hello.to_bytes(now)?).await?;
    send.finish()?;
    protocol::AgentAttachmentAck::from_bytes(
        &recv
            .read_to_end(protocol::MAX_AGENT_ATTACHMENT_BYTES)
            .await?,
        now,
    )?;
    Ok(connection)
}

/// The relay only works if the hello carries an address the holder can dial,
/// so the record is built from the agent's real bound addresses.
fn signed_hello(agent: &Endpoint, now: u64) -> Result<AgentAttachmentHello> {
    let (public, private) = crypto::ensure_keypair_ephemeral()?;
    let direct_addresses: Vec<String> = agent
        .addr()
        .ip_addrs()
        .map(|address: &SocketAddr| address.to_string())
        .collect();
    ensure!(
        !direct_addresses.is_empty(),
        "test agent endpoint published no direct address"
    );
    let record = EndpointRecord {
        version: ENDPOINT_RECORD_VERSION,
        endpoint_id: agent.id().as_bytes().to_vec(),
        relay_url: None,
        direct_addresses,
        signing_pubkey: String::new(),
        issued_at_secs: now,
        expires_at_secs: now + 60,
        signature: String::new(),
    }
    .sign(&public, &private, now)?;
    AgentAttachmentHello {
        version: SCHEDULER_MESH_PROTOCOL_VERSION,
        role: protocol::MachineRole::Agent,
        agent_endpoint: record,
        nonce: "control-relay-nonce".into(),
        issued_at_secs: now,
        expires_at_secs: now + 60,
        signing_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(&public, &private, now)
}

async fn wait_for_attachment(attachments: &AttachmentManager, agent: EndpointId) -> Result<()> {
    timeout(TEST_TIMEOUT, async {
        while attachments.agent_addr(agent).await.is_none() {
            tokio::time::sleep(ATTACHMENT_POLL_INTERVAL).await;
        }
    })
    .await
    .context("agent never became attached")
}
