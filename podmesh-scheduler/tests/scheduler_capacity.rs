mod common;

use std::{collections::HashSet, net::SocketAddr, time::Duration};

use anyhow::{Context, Result, ensure};
use common::{TEST_TIMEOUT, config, endpoint, now_secs, signed_query};
use iroh::{EndpointAddr, SecretKey, address_lookup::memory::MemoryLookup};
use podmesh_scheduler::machine::{
    AttachmentManager, CapacityCriteria, PlacementHandler, QueryManager, SchedulerGossip,
    SchedulerIdentity,
};
use protocol::{
    AGENT_CAPACITY_ALPN, AgentAttachmentHello, CAPACITY_OFFER_ALPN, CAPACITY_PROTOCOL_VERSION,
    CapacityOffer, CapacityQuery, ENDPOINT_RECORD_VERSION, EndpointRecord,
    MAX_CAPACITY_MESSAGE_BYTES, SCHEDULER_MESH_PROTOCOL_VERSION,
};
use tokio::time::timeout;

#[tokio::test]
async fn authenticated_agent_attachment_receives_queries_and_cleans_up() -> Result<()> {
    let lookup = MemoryLookup::new();
    let scheduler = endpoint(&SecretKey::generate(), &lookup).await?;
    let agent = endpoint(&SecretKey::generate(), &lookup).await?;
    lookup.add_endpoint_info(scheduler.addr());
    lookup.add_endpoint_info(agent.addr());

    let machine_config = config(HashSet::from([scheduler.id()]), Vec::new());
    let attachments = AttachmentManager::new(4, 4, TEST_TIMEOUT);
    let queries = QueryManager::new(4, 4, TEST_TIMEOUT);
    let gossip = SchedulerGossip::start(
        scheduler.clone(),
        &machine_config,
        attachments.handler(),
        queries.offer_handler(),
        PlacementHandler::new(4, TEST_TIMEOUT),
    )
    .await?;
    let connection = agent.connect(scheduler.addr(), AGENT_CAPACITY_ALPN).await?;
    let now = now_secs();
    let hello = signed_hello(agent.id().as_bytes(), now)?;
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&hello.to_bytes(now)?).await?;
    send.finish()?;
    let attachment_ack = protocol::AgentAttachmentAck::from_bytes(
        &recv
            .read_to_end(protocol::MAX_AGENT_ATTACHMENT_BYTES)
            .await?,
        now,
    )?;
    ensure!(
        attachment_ack.relay_grants.is_empty(),
        "test scheduler unexpectedly issued a relay grant"
    );
    wait_for_attachments(&attachments, 1).await?;

    let query = signed_query(scheduler.id().as_bytes(), now)?;
    ensure!(
        attachments.fanout(&query).await? == 1,
        "query was not sent to attached agent"
    );
    let mut query_stream = timeout(TEST_TIMEOUT, connection.accept_uni())
        .await
        .context("agent did not receive capacity stream")??;
    let query_bytes = query_stream.read_to_end(MAX_CAPACITY_MESSAGE_BYTES).await?;
    ensure!(
        CapacityQuery::from_bytes(&query_bytes, now)? == query,
        "agent received another capacity query"
    );

    connection.close(0u8.into(), b"test complete");
    wait_for_attachments(&attachments, 0).await?;
    timeout(TEST_TIMEOUT, gossip.shutdown()).await??;
    timeout(TEST_TIMEOUT, agent.close()).await?;
    timeout(TEST_TIMEOUT, scheduler.close()).await?;
    Ok(())
}

#[tokio::test]
async fn attachment_hello_cannot_name_another_endpoint() -> Result<()> {
    let lookup = MemoryLookup::new();
    let scheduler = endpoint(&SecretKey::generate(), &lookup).await?;
    let agent = endpoint(&SecretKey::generate(), &lookup).await?;
    lookup.add_endpoint_info(scheduler.addr());
    lookup.add_endpoint_info(agent.addr());
    let machine_config = config(HashSet::from([scheduler.id()]), Vec::new());
    let attachments = AttachmentManager::new(4, 4, TEST_TIMEOUT);
    let queries = QueryManager::new(4, 4, TEST_TIMEOUT);
    let gossip = SchedulerGossip::start(
        scheduler.clone(),
        &machine_config,
        attachments.handler(),
        queries.offer_handler(),
        PlacementHandler::new(4, TEST_TIMEOUT),
    )
    .await?;

    let connection = agent.connect(scheduler.addr(), AGENT_CAPACITY_ALPN).await?;
    let hello = signed_hello(SecretKey::generate().public().as_bytes(), now_secs())?;
    let (mut send, _recv) = connection.open_bi().await?;
    send.write_all(&hello.to_bytes(now_secs())?).await?;
    send.finish()?;
    timeout(TEST_TIMEOUT, connection.closed())
        .await
        .context("mismatched attachment connection was not closed")?;
    ensure!(
        attachments.len().await == 0,
        "mismatched agent was attached"
    );

    timeout(TEST_TIMEOUT, gossip.shutdown()).await??;
    timeout(TEST_TIMEOUT, agent.close()).await?;
    timeout(TEST_TIMEOUT, scheduler.close()).await?;
    Ok(())
}

#[tokio::test]
async fn direct_signed_offer_reaches_pending_query() -> Result<()> {
    let lookup = MemoryLookup::new();
    let scheduler = endpoint(&SecretKey::generate(), &lookup).await?;
    let agent_secret = SecretKey::generate();
    let agent = endpoint(&agent_secret, &lookup).await?;
    lookup.add_endpoint_info(scheduler.addr());
    lookup.add_endpoint_info(agent.addr());
    let machine_config = config(HashSet::from([scheduler.id()]), Vec::new());
    let attachments = AttachmentManager::new(4, 4, TEST_TIMEOUT);
    let queries = QueryManager::new(4, 4, Duration::from_secs(5));
    let gossip = SchedulerGossip::start(
        scheduler.clone(),
        &machine_config,
        attachments.handler(),
        queries.offer_handler(),
        PlacementHandler::new(4, TEST_TIMEOUT),
    )
    .await?;
    let temp = tempfile::tempdir()?;
    let identity = SchedulerIdentity::load(temp.path())?;
    let reply_address = EndpointAddr::new(identity.endpoint_id())
        .with_ip_addr(SocketAddr::from(([127, 0, 0, 1], 4300)));
    let now = now_secs();
    let begun = queries
        .begin(
            CapacityCriteria {
                cpu_milli: 500,
                memory_bytes: 512 * 1024 * 1024,
                storage_bytes: 1024 * 1024 * 1024,
                required_capabilities: vec!["linux".into()],
                excluded_endpoint_ids: Vec::new(),
            },
            &identity,
            &reply_address,
            now,
        )
        .await?;
    let offer = signed_offer(&begun.query.query_id, &agent_secret, now)?;
    let connection = agent.connect(scheduler.addr(), CAPACITY_OFFER_ALPN).await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&offer.to_bytes(now)?).await?;
    send.finish()?;
    ensure!(
        recv.read_to_end(2).await? == b"ok",
        "invalid offer acknowledgement"
    );
    connection.close(0u8.into(), b"offer delivered");
    ensure!(
        queries.finish(&begun.query.query_id, now).await == Some(offer),
        "pending query did not receive direct offer"
    );

    timeout(TEST_TIMEOUT, gossip.shutdown()).await??;
    timeout(TEST_TIMEOUT, agent.close()).await?;
    timeout(TEST_TIMEOUT, scheduler.close()).await?;
    Ok(())
}

fn signed_hello(endpoint_id: &[u8; 32], now: u64) -> Result<AgentAttachmentHello> {
    let (public, private) = crypto::ensure_keypair_ephemeral()?;
    let endpoint = signed_endpoint(endpoint_id, &public, &private, now, 60)?;
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

fn signed_offer(query_id: &str, transport: &SecretKey, now: u64) -> Result<CapacityOffer> {
    let (public, private) = crypto::ensure_keypair_ephemeral()?;
    let endpoint = signed_endpoint(transport.public().as_bytes(), &public, &private, now, 10)?;
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
    endpoint_id: &[u8; 32],
    public: &[u8],
    private: &[u8],
    now: u64,
    lifetime_secs: u64,
) -> Result<EndpointRecord> {
    EndpointRecord {
        version: ENDPOINT_RECORD_VERSION,
        endpoint_id: endpoint_id.to_vec(),
        relay_url: None,
        direct_addresses: vec!["127.0.0.1:4200".into()],
        signing_pubkey: String::new(),
        issued_at_secs: now,
        expires_at_secs: now + lifetime_secs,
        signature: String::new(),
    }
    .sign(public, private, now)
}

async fn wait_for_attachments(manager: &AttachmentManager, expected: usize) -> Result<()> {
    timeout(TEST_TIMEOUT, async {
        loop {
            if manager.len().await == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("attachment count did not converge")
}
