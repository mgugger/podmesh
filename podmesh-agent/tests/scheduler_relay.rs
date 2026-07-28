//! End-to-end proof of the client path: `podctl`-style HTTP against a
//! scheduler, relayed over Iroh to an attached agent.
//!
//! `podctl` is a plain CLI with no Iroh endpoint of its own, so this is the
//! only way an owner reaches an agent. The scheduler answers placement from the
//! mesh and then carries opaque owner-encrypted bytes to the selected agent.

use std::{collections::HashSet, future::IntoFuture, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use podmesh_agent::{
    AgentService, Config,
    config::RuntimeKind,
    machine::{AgentMachine, MachineConfig},
    runtime::MockRuntime,
};
use podmesh_scheduler::{
    clientapi::ClientApi,
    machine::{
        AgentControlForwarder, AttachmentManager, CapacityCoordinator, PlacementHandler,
        QueryManager, SchedulerGossip, SchedulerIdentity, ValidatedMachineConfig,
    },
};
use protocol::AgentControlOperation;
use tokio::time::timeout;

#[path = "iroh_capacity/control_lifecycle.rs"]
mod control_lifecycle;
use control_lifecycle::{ControlTransport, exercise_control_lifecycle};

const TEST_TIMEOUT: Duration = Duration::from_secs(20);
const RELAY_URL: &str = "https://relay.example.test/";
const MAX_CONCURRENT_RELAYS: usize = 8;

/// Speaks the scheduler's client HTTP API exactly as `podctl` does.
struct SchedulerHttpTransport {
    client: reqwest::Client,
    api_base: String,
    agent_endpoint_id: String,
}

impl ControlTransport for SchedulerHttpTransport {
    async fn send(
        &self,
        operation: AgentControlOperation,
        encrypted_payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let path = match operation {
            AgentControlOperation::Admission => "admission",
            AgentControlOperation::Deploy => "deploy",
            AgentControlOperation::Command => "command",
        };
        let response = self
            .client
            .post(format!(
                "{}/api/v1/agents/{}/{path}",
                self.api_base, self.agent_endpoint_id
            ))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(encrypted_payload)
            .send()
            .await?;
        let status = response.status();
        let body = response.bytes().await?;
        ensure!(
            status.is_success(),
            "scheduler relay of {path} failed: {status} {}",
            String::from_utf8_lossy(&body)
        );
        Ok(body.to_vec())
    }
}

#[tokio::test]
async fn podctl_http_reaches_the_agent_through_the_scheduler() -> Result<()> {
    let scheduler_temp = tempfile::tempdir()?;
    let scheduler_identity = SchedulerIdentity::load(scheduler_temp.path())?;
    let scheduler_config = ValidatedMachineConfig {
        bind_addr: "127.0.0.1:0".parse()?,
        relay_urls: vec![RELAY_URL.into()],
        relay_ca_certificates: Vec::new(),
        scheduler_members: HashSet::from([scheduler_identity.endpoint_id()]),
        scheduler_bootstraps: Vec::new(),
        query_timeout: Duration::from_secs(2),
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
    let queries = QueryManager::new(8, 8, Duration::from_secs(2));
    let gossip = SchedulerGossip::start(
        scheduler_endpoint.clone(),
        &scheduler_config,
        attachments.handler(),
        queries.offer_handler(),
        PlacementHandler::new(8, TEST_TIMEOUT),
    )
    .await?;
    let (capacity, coordinator) = CapacityCoordinator::start(
        scheduler_identity.clone(),
        scheduler_endpoint.clone(),
        queries,
        attachments.clone(),
        &gossip,
        &scheduler_config,
    );
    let forwarder = AgentControlForwarder::new(
        scheduler_endpoint.clone(),
        attachments.clone(),
        TEST_TIMEOUT,
        MAX_CONCURRENT_RELAYS,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let api_base = format!("http://{}", listener.local_addr()?);
    let http = tokio::spawn(
        axum::serve(
            listener,
            ClientApi::new(
                capacity,
                forwarder,
                scheduler_identity,
                scheduler_endpoint.clone(),
            )
            .router(),
        )
        .into_future(),
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
            operation_timeout_secs: 10,
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
        while attachments.len().await != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("agent did not attach to scheduler")?;

    let client = reqwest::Client::builder().timeout(TEST_TIMEOUT).build()?;
    let offer = client
        .get(format!("{api_base}/api/v1/agents/select"))
        .send()
        .await?
        .error_for_status()?
        .json::<protocol::CapacityOffer>()
        .await?;
    offer.verify(now_secs())?;
    ensure!(
        offer.agent_endpoint.endpoint_id == machine.endpoint().id().as_bytes(),
        "scheduler selected an agent that is not the attached one"
    );

    let transport = SchedulerHttpTransport {
        client: client.clone(),
        api_base: api_base.clone(),
        agent_endpoint_id: hex::encode(&offer.agent_endpoint.endpoint_id),
    };
    exercise_control_lifecycle(&transport, &offer).await?;

    let unknown_agent = hex::encode(iroh::SecretKey::generate().public().as_bytes());
    let rejected = client
        .post(format!("{api_base}/api/v1/agents/{unknown_agent}/command"))
        .body(vec![1u8; 32])
        .send()
        .await?;
    ensure!(
        rejected.status() == reqwest::StatusCode::NOT_FOUND,
        "scheduler must not relay to an unattached agent"
    );

    let malformed = client
        .post(format!("{api_base}/api/v1/agents/not-an-endpoint/command"))
        .body(vec![1u8; 32])
        .send()
        .await?;
    ensure!(
        malformed.status() == reqwest::StatusCode::BAD_REQUEST,
        "scheduler must reject malformed agent identifiers"
    );

    http.abort();
    machine.shutdown().await?;
    coordinator.shutdown().await?;
    gossip.shutdown().await?;
    scheduler_endpoint.close().await;
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
