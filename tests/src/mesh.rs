//! In-process mesh harness: one scheduler plus N agents, wired exactly the way
//! the real deployment is.
//!
//! Agents do not gossip. Each agent opens a persistent authenticated Iroh
//! connection to the scheduler on `AGENT_CAPACITY_ALPN`, sends a signed
//! `AgentAttachmentHello`, and from then on answers the capacity queries the
//! scheduler pushes down that attachment. One scheduler can hold many agents,
//! which is what lets a single `podctl apply` spread replicas across hosts.
//!
//! Tests drive this harness through the scheduler's client HTTP API, the same
//! surface `podctl` uses.

use std::{collections::HashSet, future::IntoFuture, sync::Arc, time::Duration};

use anyhow::{Context, Result};
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
use tokio::time::timeout;

/// Generous but bounded: every wait in this harness must fail loudly instead of
/// hanging a test run.
pub const MESH_TIMEOUT: Duration = Duration::from_secs(30);
const RELAY_URL: &str = "https://relay.example.test/";
const MAX_CONCURRENT_RELAYS: usize = 16;
const MAX_ATTACHED_AGENTS: usize = 16;
const MAX_AGENT_FANOUT: usize = 16;
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const AGENT_CPU_MILLI: u32 = 8_000;
const AGENT_MEMORY_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const AGENT_STORAGE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const AGENT_MAX_WORKLOADS: usize = 16;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// One agent attached to the harness scheduler.
pub struct TestAgent {
    /// Lowercase hex Iroh EndpointId, the form `podctl` addresses agents by.
    pub endpoint_id: String,
    /// Records the manifests the agent actually deployed, after sidecar
    /// injection.
    pub runtime: Arc<MockRuntime>,
    _machine: AgentMachine,
    _temp: tempfile::TempDir,
}

/// A scheduler with `agent_count` agents attached to it.
pub struct TestMesh {
    /// Base URL of the scheduler's client HTTP API.
    pub api_base: String,
    pub agents: Vec<TestAgent>,
    _gossip: SchedulerGossip,
    _coordinator: CapacityCoordinator,
    _http: tokio::task::JoinHandle<std::io::Result<()>>,
    _temp: tempfile::TempDir,
}

impl TestMesh {
    /// Starts a scheduler and waits until exactly `agent_count` agents have
    /// attached, so placement is deterministic once this returns.
    pub async fn start(agent_count: usize) -> Result<Self> {
        anyhow::ensure!(
            agent_count >= 1 && agent_count <= MAX_ATTACHED_AGENTS,
            "agent_count must be between 1 and {MAX_ATTACHED_AGENTS}"
        );
        let temp = tempfile::tempdir()?;
        let identity = SchedulerIdentity::load(temp.path())?;
        let config = ValidatedMachineConfig {
            bind_addr: "127.0.0.1:0".parse()?,
            relay_urls: vec![RELAY_URL.into()],
            relay_ca_certificates: Vec::new(),
            scheduler_members: HashSet::from([identity.endpoint_id()]),
            scheduler_bootstraps: Vec::new(),
            query_timeout: QUERY_TIMEOUT,
            max_pending_queries: MAX_ATTACHED_AGENTS,
            max_seen_queries: 64,
            max_attached_agents: MAX_ATTACHED_AGENTS,
            max_offers_per_query: MAX_ATTACHED_AGENTS,
            max_agent_fanout: MAX_AGENT_FANOUT,
        };
        let endpoint = identity.bind_endpoint(&config, now_secs()).await?;
        let record = identity.endpoint_record(&endpoint.addr(), now_secs(), now_secs() + 300)?;
        let attachments =
            AttachmentManager::new(MAX_ATTACHED_AGENTS, MAX_AGENT_FANOUT, MESH_TIMEOUT)
                .with_relay_grant_issuer(identity.clone(), RELAY_URL.into());
        let queries = QueryManager::new(MAX_ATTACHED_AGENTS, MAX_ATTACHED_AGENTS, QUERY_TIMEOUT);
        let gossip = SchedulerGossip::start(
            endpoint.clone(),
            &config,
            attachments.handler(),
            queries.offer_handler(),
            PlacementHandler::new(MAX_ATTACHED_AGENTS, MESH_TIMEOUT),
        )
        .await?;
        let (capacity, coordinator) = CapacityCoordinator::start(
            identity.clone(),
            endpoint.clone(),
            queries,
            attachments.clone(),
            &gossip,
            &config,
        );
        let forwarder = AgentControlForwarder::new(
            endpoint.clone(),
            attachments.clone(),
            MESH_TIMEOUT,
            MAX_CONCURRENT_RELAYS,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let api_base = format!("http://{}", listener.local_addr()?);
        let http = tokio::spawn(
            axum::serve(
                listener,
                ClientApi::new(capacity, forwarder, identity, endpoint.clone()).router(),
            )
            .into_future(),
        );

        let scheduler_endpoint = crypto::b64_encode(&record.to_bytes(now_secs())?);
        let mut agents = Vec::with_capacity(agent_count);
        for _ in 0..agent_count {
            agents.push(start_agent(&scheduler_endpoint).await?);
        }
        timeout(MESH_TIMEOUT, async {
            while attachments.len().await != agent_count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| format!("only some of the {agent_count} agents attached"))?;

        Ok(Self {
            api_base,
            agents,
            _gossip: gossip,
            _coordinator: coordinator,
            _http: http,
            _temp: temp,
        })
    }

    /// The agent addressed by this lowercase hex EndpointId.
    pub fn agent(&self, endpoint_id: &str) -> Option<&TestAgent> {
        self.agents
            .iter()
            .find(|agent| agent.endpoint_id == endpoint_id)
    }

    /// Total workloads currently deployed across every agent in the mesh.
    pub async fn total_deployed_workloads(&self) -> usize {
        let mut total = 0;
        for agent in &self.agents {
            total += agent.runtime.deployed_workload_ids().await.len();
        }
        total
    }
}

async fn start_agent(scheduler_endpoint: &str) -> Result<TestAgent> {
    let temp = tempfile::tempdir()?;
    let config = Config {
        listen: "127.0.0.1:0".into(),
        key_dir: temp.path().join("keys"),
        state_path: temp.path().join("state.redb"),
        runtime: RuntimeKind::Mock,
        workload_network: "podmesh".into(),
        sidecar_image: "podmesh/sidecar:latest".into(),
        capacity_cpu_milli: AGENT_CPU_MILLI,
        capacity_memory_bytes: AGENT_MEMORY_BYTES,
        capacity_storage_bytes: AGENT_STORAGE_BYTES,
        max_workloads: AGENT_MAX_WORKLOADS,
        machine: MachineConfig {
            bind_addr: "127.0.0.1:0".parse()?,
            scheduler_endpoints: vec![scheduler_endpoint.to_string()],
            scheduler_urls: Vec::new(),
            relay_urls: vec![RELAY_URL.into()],
            relay_ca_certificate_paths: Vec::new(),
            max_scheduler_attachments: 1,
            reconnect_initial_ms: 25,
            reconnect_max_ms: 100,
            max_seen_queries: 64,
            operation_timeout_secs: 10,
            max_concurrent_uni_streams: 16,
            max_concurrent_bidi_streams: 8,
            max_idle_secs: 180,
            stream_receive_window_bytes: 64 * 1024,
            connection_receive_window_bytes: 1024 * 1024,
        },
    };
    let runtime = Arc::new(MockRuntime::default());
    let service = AgentService::new(config.clone(), runtime.clone()).await?;
    let machine = AgentMachine::start(&config, service).await?;
    Ok(TestAgent {
        endpoint_id: hex::encode(machine.endpoint().id().as_bytes()),
        runtime,
        _machine: machine,
        _temp: temp,
    })
}

/// A signed proxy `EndpointRecord` for tests that only need injection to carry
/// a well-formed discovery seed rather than a live proxy.
pub fn test_proxy_endpoint() -> Result<protocol::EndpointRecord> {
    let now = now_secs();
    let (public, private) = crypto::ensure_keypair_ephemeral()?;
    protocol::EndpointRecord {
        version: protocol::ENDPOINT_RECORD_VERSION,
        endpoint_id: iroh::SecretKey::generate().public().as_bytes().to_vec(),
        relay_url: Some(RELAY_URL.into()),
        direct_addresses: vec!["127.0.0.1:4002".into()],
        signing_pubkey: String::new(),
        issued_at_secs: now,
        expires_at_secs: now + 300,
        signature: String::new(),
    }
    .sign(&public, &private, now)
}
