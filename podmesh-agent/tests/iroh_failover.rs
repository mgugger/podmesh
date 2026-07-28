use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use podmesh_agent::{
    AgentService, Config,
    config::RuntimeKind,
    machine::{AgentMachine, MachineConfig},
    runtime::MockRuntime,
};
use podmesh_scheduler::machine::{
    AttachmentManager, CapacityCoordinator, CapacityCriteria, PlacementHandler, QueryManager,
    SchedulerGossip, SchedulerIdentity, ValidatedMachineConfig,
};
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const RELAY_URL: &str = "https://relay.example.test/";

struct SchedulerNode {
    endpoint: iroh::Endpoint,
    attachments: AttachmentManager,
    gossip: SchedulerGossip,
    coordinator: CapacityCoordinator,
    capacity: podmesh_scheduler::machine::CapacityService,
    record: protocol::EndpointRecord,
}

#[tokio::test]
async fn surviving_scheduler_continues_capacity_after_peer_loss() -> Result<()> {
    let first = scheduler_node().await?;
    let second = scheduler_node().await?;
    let agent_temp = tempfile::tempdir()?;
    let config = agent_config(
        agent_temp.path(),
        &[first.record.clone(), second.record.clone()],
    )?;
    let service = AgentService::new(config.clone(), Arc::new(MockRuntime::default())).await?;
    let machine = AgentMachine::start(&config, service).await?;
    wait_for_attachments(&first.attachments, 1).await?;
    wait_for_attachments(&second.attachments, 1).await?;

    first.shutdown().await?;
    let offer = second
        .capacity
        .solicit(CapacityCriteria {
            cpu_milli: 500,
            memory_bytes: 512 * 1024 * 1024,
            storage_bytes: 1024 * 1024 * 1024,
            required_capabilities: vec!["multi-workload".into()],
            excluded_endpoint_ids: Vec::new(),
        })
        .await?
        .context("surviving scheduler received no offer")?;
    offer.verify(now_secs())?;
    ensure!(
        offer.agent_endpoint.endpoint_id == machine.endpoint().id().as_bytes(),
        "surviving scheduler received an offer from another endpoint"
    );

    timeout(TEST_TIMEOUT, machine.shutdown()).await??;
    second.shutdown().await?;
    Ok(())
}

async fn scheduler_node() -> Result<SchedulerNode> {
    let temp = tempfile::tempdir()?;
    let identity = SchedulerIdentity::load(temp.path())?;
    let config = ValidatedMachineConfig {
        bind_addr: "127.0.0.1:0".parse()?,
        relay_urls: vec![RELAY_URL.into()],
        relay_ca_certificates: Vec::new(),
        scheduler_members: HashSet::from([identity.endpoint_id()]),
        scheduler_bootstraps: Vec::new(),
        query_timeout: Duration::from_secs(1),
        max_pending_queries: 8,
        max_seen_queries: 16,
        max_attached_agents: 8,
        max_offers_per_query: 8,
        max_agent_fanout: 8,
    };
    let endpoint = identity.bind_endpoint(&config, now_secs()).await?;
    let record = identity.endpoint_record(&endpoint.addr(), now_secs(), now_secs() + 300)?;
    let attachments = AttachmentManager::new(8, 8, TEST_TIMEOUT)
        .with_relay_grant_issuer(identity.clone(), RELAY_URL.into());
    let queries = QueryManager::new(8, 8, Duration::from_secs(1));
    let gossip = SchedulerGossip::start(
        endpoint.clone(),
        &config,
        attachments.handler(),
        queries.offer_handler(),
        PlacementHandler::new(8, TEST_TIMEOUT),
    )
    .await?;
    let (capacity, coordinator) = CapacityCoordinator::start(
        identity,
        endpoint.clone(),
        queries,
        attachments.clone(),
        &gossip,
        &config,
    );
    Ok(SchedulerNode {
        endpoint,
        attachments,
        gossip,
        coordinator,
        capacity,
        record,
    })
}

impl SchedulerNode {
    async fn shutdown(self) -> Result<()> {
        timeout(TEST_TIMEOUT, self.coordinator.shutdown()).await??;
        timeout(TEST_TIMEOUT, self.gossip.shutdown()).await??;
        timeout(TEST_TIMEOUT, self.endpoint.close()).await?;
        Ok(())
    }
}

fn agent_config(root: &std::path::Path, records: &[protocol::EndpointRecord]) -> Result<Config> {
    Ok(Config {
        listen: "127.0.0.1:0".into(),
        key_dir: root.join("keys"),
        state_path: root.join("state.redb"),
        runtime: RuntimeKind::Mock,
        workload_network: "podmesh".into(),
        sidecar_image: "podmesh/sidecar:latest".into(),
        capacity_cpu_milli: 2_000,
        capacity_memory_bytes: 2 * 1024 * 1024 * 1024,
        capacity_storage_bytes: 10 * 1024 * 1024 * 1024,
        max_workloads: 8,
        machine: MachineConfig {
            bind_addr: "127.0.0.1:0".parse()?,
            scheduler_endpoints: records
                .iter()
                .map(|record| Ok(crypto::b64_encode(&record.to_bytes(now_secs())?)))
                .collect::<Result<Vec<_>>>()?,
            scheduler_urls: Vec::new(),
            relay_urls: vec![RELAY_URL.into()],
            relay_ca_certificate_paths: Vec::new(),
            max_scheduler_attachments: records.len(),
            reconnect_initial_ms: 25,
            reconnect_max_ms: 100,
            max_seen_queries: 32,
            operation_timeout_secs: 5,
            max_concurrent_uni_streams: 8,
            max_concurrent_bidi_streams: 4,
            max_idle_secs: 180,
            stream_receive_window_bytes: 64 * 1024,
            connection_receive_window_bytes: 1024 * 1024,
        },
    })
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
    .context("agent attachment count did not converge")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
