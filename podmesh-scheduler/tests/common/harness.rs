//! A minimal but real scheduler, for tests that need several of them.
//!
//! Everything a scheduler needs to relay owner traffic is wired up here exactly
//! as production wiring does it — one shared location registry, a gossip
//! publisher installed after startup, and a forwarder installed into the relay
//! handler — because tests that skip any of those stop exercising the path they
//! claim to test.
#![allow(dead_code)]

use std::{collections::HashSet, net::SocketAddr, time::Duration};

use anyhow::{Context, Result, ensure};
use iroh::{
    Endpoint, EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use podmesh_scheduler::machine::{
    AgentControlForwarder, AttachmentManager, LocationRegistry, MemberIssuers, MemberRegistry,
    PeerControlRelay, PlacementHandler, QueryManager, SchedulerGossip, SchedulerGossipServices,
    SchedulerIdentity,
};
use protocol::{
    AGENT_CAPACITY_ALPN, AgentAttachmentHello, AgentControlRequest, AgentControlResponse,
    ENDPOINT_RECORD_VERSION, EndpointRecord, MAX_AGENT_CONTROL_FRAME_BYTES,
    SCHEDULER_MESH_PROTOCOL_VERSION,
};
use tokio::time::timeout;

use super::{TEST_TIMEOUT, config, now_secs};

const MAX_CONCURRENT_RELAYS: usize = 4;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long a test announcement claims to be valid.
const TEST_ANNOUNCEMENT_LIFETIME_SECS: u64 = 300;

/// Fake agent control endpoint: it never decrypts anything, it only proves the
/// owner payload arrived unchanged and that its answer travels back.
#[derive(Debug, Clone)]
pub struct EchoAgentControl;

impl ProtocolHandler for EchoAgentControl {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.echo(&connection)
            .await
            .map_err(|error| AcceptError::from_err(std::io::Error::other(error.to_string())))?;
        connection.closed().await;
        Ok(())
    }
}

impl EchoAgentControl {
    async fn echo(&self, connection: &Connection) -> Result<()> {
        let (mut send, mut recv) = connection.accept_bi().await?;
        let bytes = recv.read_to_end(MAX_AGENT_CONTROL_FRAME_BYTES).await?;
        let mut echoed = AgentControlRequest::from_bytes(&bytes)?.encrypted_payload;
        echoed.reverse();
        send.write_all(&AgentControlResponse::success(echoed).to_bytes()?)
            .await?;
        send.finish()?;
        Ok(())
    }
}

pub struct TestScheduler {
    pub endpoint: Endpoint,
    pub gossip: SchedulerGossip,
    pub attachments: AttachmentManager,
    pub forwarder: AgentControlForwarder,
    pub locations: LocationRegistry,
    identity: SchedulerIdentity,
    _key_dir: tempfile::TempDir,
}

impl TestScheduler {
    pub async fn start(
        endpoint: Endpoint,
        identity: SchedulerIdentity,
        key_dir: tempfile::TempDir,
        members: HashSet<EndpointId>,
        bootstraps: Vec<EndpointId>,
    ) -> Result<Self> {
        let machine_config = config(members, bootstraps);
        let attachments = AttachmentManager::new(4, 4, TEST_TIMEOUT);
        let queries = QueryManager::new(4, 4, TEST_TIMEOUT);
        // One registry: the gossip receiver resolves the queries the relay
        // opened, so they must share it.
        let locations = LocationRegistry::new();
        let gossip = SchedulerGossip::start(
            SchedulerGossipServices {
                endpoint: endpoint.clone(),
                attachments: attachments.handler(),
                offers: queries.offer_handler(),
                placement: PlacementHandler::new(4, TEST_TIMEOUT),
                locations: locations.clone(),
                member_issuers: MemberIssuers::new(),
                lookup: identity.peer_lookup(),
            },
            &machine_config,
        )
        .await?;
        let peer_relay = PeerControlRelay::new(
            endpoint.clone(),
            gossip.members(),
            locations.clone(),
            identity.clone(),
            TEST_TIMEOUT,
        );
        peer_relay.install_publisher(gossip.publisher())?;
        let forwarder = AgentControlForwarder::new(
            endpoint.clone(),
            attachments.clone(),
            peer_relay,
            TEST_TIMEOUT,
            MAX_CONCURRENT_RELAYS,
        );
        gossip.control_relay().install(forwarder.clone())?;
        Ok(Self {
            endpoint,
            gossip,
            attachments,
            forwarder,
            locations,
            identity,
            _key_dir: key_dir,
        })
    }

    pub fn members(&self) -> MemberRegistry {
        self.gossip.members()
    }

    /// Announce this scheduler on the mesh, as the discovery loop does.
    pub async fn announce(&self) -> Result<()> {
        let now = now_secs();
        let record = self.identity.endpoint_record(
            &self.endpoint.addr(),
            now,
            now + TEST_ANNOUNCEMENT_LIFETIME_SECS,
        )?;
        self.gossip.publisher().publish_announcement(record).await
    }

    pub async fn shutdown(self) -> Result<()> {
        timeout(TEST_TIMEOUT, self.gossip.shutdown()).await??;
        timeout(TEST_TIMEOUT, self.endpoint.close()).await?;
        Ok(())
    }
}

pub async fn attach(agent: &Endpoint, scheduler: &Endpoint) -> Result<Connection> {
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
    let (public, private) = crypto::generate_signing_keypair();
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

pub async fn wait_for_attachment(attachments: &AttachmentManager, agent: EndpointId) -> Result<()> {
    timeout(TEST_TIMEOUT, async {
        while attachments.agent_addr(agent).await.is_none() {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("agent never became attached")
}

pub async fn wait_for_member(members: &MemberRegistry, peer: EndpointId) -> Result<()> {
    timeout(TEST_TIMEOUT, async {
        while !members.contains(&peer) {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("scheduler was never admitted into the mesh")
}
