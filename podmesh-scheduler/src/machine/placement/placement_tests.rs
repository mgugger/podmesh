use std::{collections::HashSet, net::SocketAddr, time::Duration};

use anyhow::{Context, Result, ensure};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, SecretKey, address_lookup::memory::MemoryLookup,
    endpoint::presets,
};
use protocol::{
    AGENT_CAPACITY_ALPN, AgentAttachmentHello, CAPACITY_OFFER_ALPN, CAPACITY_PROTOCOL_VERSION,
    CapacityOffer, CapacityQuery, ENDPOINT_RECORD_VERSION, EndpointRecord,
    MAX_CAPACITY_MESSAGE_BYTES, PLACEMENT_PROTOCOL_VERSION, PlacementRequest, PlacementResponse,
    SCHEDULER_MESH_PROTOCOL_VERSION, SCHEDULER_PLACEMENT_ALPN,
};

use super::now_secs;
use crate::machine::{
    AttachmentManager, CapacityCoordinator, PlacementHandler, QueryManager, SchedulerGossip,
    SchedulerIdentity, ValidatedMachineConfig,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn placement_stream_solicits_attached_agent_and_returns_direct_offer() -> Result<()> {
    let lookup = MemoryLookup::new();
    let identity = SchedulerIdentity::ephemeral()?;
    let scheduler = endpoint(identity.transport_secret(), &lookup).await?;
    let agent_secret = SecretKey::generate();
    let agent = endpoint(agent_secret.clone(), &lookup).await?;
    let client = endpoint(SecretKey::generate(), &lookup).await?;
    for endpoint in [&scheduler, &agent, &client] {
        lookup.add_endpoint_info(endpoint.addr());
    }
    let config = ValidatedMachineConfig {
        bind_addr: "127.0.0.1:0".parse()?,
        relay_urls: vec!["https://relay.example.test".into()],
        relay_ca_certificates: Vec::new(),
        scheduler_members: HashSet::from([scheduler.id()]),
        scheduler_bootstraps: Vec::new(),
        query_timeout: Duration::from_secs(1),
        max_pending_queries: 8,
        max_seen_queries: 16,
        max_attached_agents: 8,
        max_offers_per_query: 8,
        max_agent_fanout: 8,
    };
    let attachments = AttachmentManager::new(8, 8, TEST_TIMEOUT);
    let queries = QueryManager::new(8, 8, Duration::from_secs(1));
    let placement = PlacementHandler::new(8, TEST_TIMEOUT);
    let gossip = SchedulerGossip::start(
        scheduler.clone(),
        &config,
        attachments.handler(),
        queries.offer_handler(),
        placement.clone(),
    )
    .await?;
    let (service, coordinator) = CapacityCoordinator::start(
        identity,
        scheduler.clone(),
        queries,
        attachments.clone(),
        &gossip,
        &config,
    );
    placement.install(service)?;

    let attachment = agent.connect(scheduler.addr(), AGENT_CAPACITY_ALPN).await?;
    let now = now_secs();
    let hello = signed_hello(agent.id(), now)?;
    let (mut hello_send, mut hello_recv) = attachment.open_bi().await?;
    hello_send.write_all(&hello.to_bytes(now)?).await?;
    hello_send.finish()?;
    let attachment_ack = protocol::AgentAttachmentAck::from_bytes(
        &hello_recv
            .read_to_end(protocol::MAX_AGENT_ATTACHMENT_BYTES)
            .await?,
        now,
    )?;
    ensure!(
        attachment_ack.relay_grants.is_empty(),
        "test scheduler unexpectedly issued a relay grant"
    );

    let agent_task = tokio::spawn({
        let agent = agent.clone();
        async move {
            let mut query_stream = attachment.accept_uni().await?;
            let bytes = query_stream.read_to_end(MAX_CAPACITY_MESSAGE_BYTES).await?;
            let query = CapacityQuery::from_bytes(&bytes, now_secs())?;
            let offer = signed_offer(&query.query_id, agent.id(), now_secs())?;
            let destination = endpoint_addr(&query.reply_endpoint)?;
            let connection = agent.connect(destination, CAPACITY_OFFER_ALPN).await?;
            let (mut send, mut recv) = connection.open_bi().await?;
            send.write_all(&offer.to_bytes(now_secs())?).await?;
            send.finish()?;
            ensure!(recv.read_to_end(2).await? == b"ok", "invalid offer ack");
            connection.close(0u8.into(), b"offer delivered");
            attachment.close(0u8.into(), b"query handled");
            Result::<()>::Ok(())
        }
    });

    let placement_connection = client
        .connect(scheduler.addr(), SCHEDULER_PLACEMENT_ALPN)
        .await?;
    let request = PlacementRequest {
        version: PLACEMENT_PROTOCOL_VERSION,
        request_id: "placement-1".into(),
        cpu_milli: 500,
        memory_bytes: 512 * 1024 * 1024,
        storage_bytes: 1024 * 1024 * 1024,
        required_capabilities: vec!["linux".into()],
        excluded_endpoint_ids: Vec::new(),
        issued_at_secs: now,
        expires_at_secs: now + 5,
    };
    let (mut send, mut recv) = placement_connection.open_bi().await?;
    send.write_all(&request.to_bytes(now)?).await?;
    send.finish()?;
    let response = PlacementResponse::from_bytes(
        &recv.read_to_end(MAX_CAPACITY_MESSAGE_BYTES).await?,
        now_secs(),
    )?;
    ensure!(
        response.offer.is_some(),
        "placement returned no capacity offer"
    );
    ensure!(response.error.is_none(), "placement returned an error");
    placement_connection.close(0u8.into(), b"response received");
    tokio::time::timeout(TEST_TIMEOUT, agent_task).await???;

    tokio::time::timeout(TEST_TIMEOUT, coordinator.shutdown()).await??;
    tokio::time::timeout(TEST_TIMEOUT, gossip.shutdown()).await??;
    tokio::time::timeout(TEST_TIMEOUT, client.close()).await?;
    tokio::time::timeout(TEST_TIMEOUT, agent.close()).await?;
    tokio::time::timeout(TEST_TIMEOUT, scheduler.close()).await?;
    Ok(())
}

async fn endpoint(secret: SecretKey, lookup: &MemoryLookup) -> Result<Endpoint> {
    Endpoint::builder(presets::Minimal)
        .clear_relay_transports()
        .secret_key(secret)
        .address_lookup(lookup.clone())
        .bind()
        .await
        .context("bind placement test endpoint")
}

fn signed_hello(endpoint_id: EndpointId, now: u64) -> Result<AgentAttachmentHello> {
    let (public, private) = crypto::ensure_keypair_ephemeral()?;
    let endpoint = signed_endpoint(endpoint_id, &public, &private, now)?;
    AgentAttachmentHello {
        version: SCHEDULER_MESH_PROTOCOL_VERSION,
        role: protocol::MachineRole::Agent,
        agent_endpoint: endpoint,
        nonce: "attachment-nonce".into(),
        issued_at_secs: now,
        expires_at_secs: now + 30,
        signing_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(&public, &private, now)
}

fn signed_offer(query_id: &str, endpoint_id: EndpointId, now: u64) -> Result<CapacityOffer> {
    let (public, private) = crypto::ensure_keypair_ephemeral()?;
    let endpoint = signed_endpoint(endpoint_id, &public, &private, now)?;
    CapacityOffer {
        version: CAPACITY_PROTOCOL_VERSION,
        query_id: query_id.into(),
        agent_endpoint: endpoint,
        kem_pubkey: crypto::b64_encode(&[7; 32]),
        available_cpu_milli: 750,
        available_memory_bytes: 1024 * 1024 * 1024,
        available_storage_bytes: 2 * 1024 * 1024 * 1024,
        capabilities: vec!["linux".into()],
        issued_at_secs: now,
        expires_at_secs: now + 10,
        signing_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(&public, &private, now)
}

fn signed_endpoint(
    endpoint_id: EndpointId,
    public: &[u8],
    private: &[u8],
    now: u64,
) -> Result<EndpointRecord> {
    EndpointRecord {
        version: ENDPOINT_RECORD_VERSION,
        endpoint_id: endpoint_id.as_bytes().to_vec(),
        relay_url: None,
        direct_addresses: vec!["127.0.0.1:4100".into()],
        signing_pubkey: String::new(),
        issued_at_secs: now,
        expires_at_secs: now + 30,
        signature: String::new(),
    }
    .sign(public, private, now)
}

fn endpoint_addr(record: &EndpointRecord) -> Result<EndpointAddr> {
    let endpoint_bytes: [u8; 32] = record.endpoint_id.as_slice().try_into()?;
    let endpoint_id = EndpointId::from_bytes(&endpoint_bytes)?;
    let addresses = record
        .direct_addresses
        .iter()
        .map(|address| address.parse::<SocketAddr>())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(addresses
        .into_iter()
        .fold(EndpointAddr::new(endpoint_id), EndpointAddr::with_ip_addr))
}
