use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, endpoint::presets};
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
use protocol::{AGENT_CONTROL_ALPN, AgentControlResponse, MAX_AGENT_CONTROL_FRAME_BYTES};
use tokio::time::timeout;

#[path = "iroh_capacity/control_lifecycle.rs"]
mod control_lifecycle;
use control_lifecycle::{ControlTransport, exercise_control_lifecycle};

/// Dials the agent's control ALPN directly. This is the transport the
/// scheduler itself uses when relaying owner traffic.
struct IrohControlTransport {
    client: Endpoint,
    agent_address: iroh::EndpointAddr,
}

impl IrohControlTransport {
    async fn connect(agent_endpoint: &Endpoint) -> Result<Self> {
        Ok(Self {
            client: Endpoint::builder(presets::Minimal)
                .clear_relay_transports()
                .bind()
                .await?,
            agent_address: agent_endpoint.addr(),
        })
    }

    async fn close(self) -> Result<()> {
        timeout(TEST_TIMEOUT, self.client.close()).await?;
        Ok(())
    }
}

impl ControlTransport for IrohControlTransport {
    async fn send(
        &self,
        operation: protocol::AgentControlOperation,
        encrypted_payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let connection = timeout(
            TEST_TIMEOUT,
            self.client
                .connect(self.agent_address.clone(), AGENT_CONTROL_ALPN),
        )
        .await
        .context("agent control connection timed out")??;
        let request = protocol::AgentControlRequest {
            version: protocol::AGENT_CONTROL_PROTOCOL_VERSION,
            operation,
            encrypted_payload,
        };
        let (mut send, mut recv) = connection.open_bi().await?;
        send.write_all(&request.to_bytes()?).await?;
        send.finish()?;
        let response = AgentControlResponse::from_bytes(
            &recv.read_to_end(MAX_AGENT_CONTROL_FRAME_BYTES).await?,
        )?;
        connection.close(0u8.into(), b"control response received");
        ensure!(response.ok, "agent rejected control request");
        Ok(response.encrypted_payload)
    }
}

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const RELAY_URL: &str = "https://relay.example.test/";

#[tokio::test]
async fn agent_attaches_receives_live_query_and_returns_signed_offer() -> Result<()> {
    let scheduler_temp = tempfile::tempdir()?;
    let scheduler_identity = SchedulerIdentity::load(scheduler_temp.path())?;
    let scheduler_config = ValidatedMachineConfig {
        bind_addr: "127.0.0.1:0".parse()?,
        relay_urls: vec![RELAY_URL.into()],
        relay_ca_certificates: Vec::new(),
        scheduler_members: HashSet::from([scheduler_identity.endpoint_id()]),
        scheduler_bootstraps: Vec::new(),
        query_timeout: Duration::from_secs(1),
        max_pending_queries: 8,
        max_seen_queries: 16,
        max_attached_agents: 8,
        max_offers_per_query: 8,
        max_agent_fanout: 8,
    };
    let scheduler_endpoint = scheduler_identity
        .bind_endpoint(&scheduler_config, now_secs())
        .await?;
    let scheduler_record = scheduler_identity.endpoint_record(
        &scheduler_endpoint.addr(),
        now_secs(),
        now_secs() + 300,
    )?;
    let attachments = AttachmentManager::new(8, 8, TEST_TIMEOUT)
        .with_relay_grant_issuer(scheduler_identity.clone(), RELAY_URL.into());
    let queries = QueryManager::new(8, 8, Duration::from_secs(1));
    let gossip = SchedulerGossip::start(
        scheduler_endpoint.clone(),
        &scheduler_config,
        attachments.handler(),
        queries.offer_handler(),
        PlacementHandler::new(8, TEST_TIMEOUT),
    )
    .await?;
    let (capacity, coordinator) = CapacityCoordinator::start(
        scheduler_identity,
        scheduler_endpoint.clone(),
        queries,
        attachments.clone(),
        &gossip,
        &scheduler_config,
    );

    let agent_temp = tempfile::tempdir()?;
    let agent_config = Config {
        listen: "127.0.0.1:0".into(),
        key_dir: agent_temp.path().join("keys"),
        state_path: agent_temp.path().join("state.redb"),
        runtime: RuntimeKind::Mock,
        workload_network: "podmesh".into(),
        sidecar_image: "podmesh/sidecar:latest".into(),
        capacity_cpu_milli: 2_000,
        capacity_memory_bytes: 2 * 1024 * 1024 * 1024,
        capacity_storage_bytes: 10 * 1024 * 1024 * 1024,
        max_workloads: 8,
        machine: MachineConfig {
            bind_addr: "127.0.0.1:0".parse()?,
            scheduler_endpoints: vec![crypto::b64_encode(&scheduler_record.to_bytes(now_secs())?)],
            scheduler_urls: Vec::new(),
            relay_urls: vec![RELAY_URL.into()],
            relay_ca_certificate_paths: Vec::new(),
            max_scheduler_attachments: 1,
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
    };
    let service = AgentService::new(agent_config.clone(), Arc::new(MockRuntime::default())).await?;
    let machine = AgentMachine::start(&agent_config, service).await?;
    timeout(TEST_TIMEOUT, async {
        loop {
            if attachments.len().await == 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("agent did not attach to scheduler")?;

    let offer = capacity
        .solicit(CapacityCriteria {
            cpu_milli: 500,
            memory_bytes: 512 * 1024 * 1024,
            storage_bytes: 1024 * 1024 * 1024,
            required_capabilities: vec!["multi-workload".into()],
            excluded_endpoint_ids: Vec::new(),
        })
        .await?
        .context("attached agent returned no capacity offer")?;
    offer.verify(now_secs())?;
    ensure!(
        offer.agent_endpoint.endpoint_id == machine.endpoint().id().as_bytes(),
        "offer EndpointId does not match agent transport"
    );
    ensure!(
        offer.available_cpu_milli == 2_000,
        "unexpected available CPU"
    );
    malformed_control_stream_is_rejected(machine.endpoint()).await?;
    let transport = IrohControlTransport::connect(machine.endpoint()).await?;
    exercise_control_lifecycle(&transport, &offer).await?;
    transport.close().await?;

    timeout(TEST_TIMEOUT, machine.shutdown()).await??;
    timeout(TEST_TIMEOUT, coordinator.shutdown()).await??;
    timeout(TEST_TIMEOUT, gossip.shutdown()).await??;
    timeout(TEST_TIMEOUT, scheduler_endpoint.close()).await?;
    Ok(())
}

async fn malformed_control_stream_is_rejected(agent_endpoint: &Endpoint) -> Result<()> {
    let client = Endpoint::builder(presets::Minimal)
        .clear_relay_transports()
        .bind()
        .await?;
    let connection = client
        .connect(agent_endpoint.addr(), AGENT_CONTROL_ALPN)
        .await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(b"not-a-postcard-frame").await?;
    send.finish()?;
    let response =
        AgentControlResponse::from_bytes(&recv.read_to_end(MAX_AGENT_CONTROL_FRAME_BYTES).await?)?;
    ensure!(!response.ok, "malformed control request was accepted");
    ensure!(
        response.encrypted_payload.is_empty(),
        "rejection leaked a response payload"
    );
    connection.close(0u8.into(), b"malformed request handled");

    let healthy = client
        .connect(agent_endpoint.addr(), AGENT_CONTROL_ALPN)
        .await?;
    healthy.close(0u8.into(), b"endpoint survived malformed stream");
    timeout(TEST_TIMEOUT, client.close()).await?;
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
