use std::future::IntoFuture;

use anyhow::Result;
use podmesh_scheduler::{
    clientapi::ClientApi,
    machine::{
        AgentControlForwarder, AttachmentManager, CapacityCoordinator, LocationRegistry,
        PeerControlRelay, PlacementHandler, QueryManager, SchedulerGossip, SchedulerGossipServices,
        SchedulerIdentity, ValidatedMachineConfig,
    },
};

use crate::mesh::{MESH_QUERY_TIMEOUT, MESH_TIMEOUT};

const RELAY_URL: &str = "https://relay.example.test/";
const MAX_CONCURRENT_RELAYS: usize = 16;
const MAX_ATTACHED_AGENTS: usize = 16;
const MAX_AGENT_FANOUT: usize = 16;

pub(crate) struct TestScheduler {
    pub api_base: String,
    pub attachments: AttachmentManager,
    pub identity: SchedulerIdentity,
    pub endpoint: iroh::Endpoint,
    pub config: ValidatedMachineConfig,
    pub _gossip: SchedulerGossip,
    pub _coordinator: CapacityCoordinator,
    pub _http: tokio::task::JoinHandle<std::io::Result<()>>,
    pub _temp: tempfile::TempDir,
}

impl TestScheduler {
    pub async fn start(
        temp: tempfile::TempDir,
        identity: SchedulerIdentity,
        endpoint: iroh::Endpoint,
        config: ValidatedMachineConfig,
    ) -> Result<Self> {
        let attachments =
            AttachmentManager::new(MAX_ATTACHED_AGENTS, MAX_AGENT_FANOUT, MESH_TIMEOUT)
                .with_relay_grant_issuer(identity.clone(), RELAY_URL.into());
        let queries =
            QueryManager::new(MAX_ATTACHED_AGENTS, MAX_ATTACHED_AGENTS, MESH_QUERY_TIMEOUT);
        let locations = LocationRegistry::new();
        let gossip = SchedulerGossip::start(
            SchedulerGossipServices {
                endpoint: endpoint.clone(),
                attachments: attachments.handler(),
                offers: queries.offer_handler(),
                placement: PlacementHandler::new(MAX_ATTACHED_AGENTS, MESH_TIMEOUT),
                locations: locations.clone(),
                member_issuers: podmesh_scheduler::machine::MemberIssuers::new(),
                lookup: identity.peer_lookup(),
            },
            &config,
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
        let peer_relay = PeerControlRelay::new(
            endpoint.clone(),
            gossip.members(),
            locations,
            identity.clone(),
            MESH_TIMEOUT,
        );
        peer_relay.install_publisher(gossip.publisher())?;
        let forwarder = AgentControlForwarder::new(
            endpoint.clone(),
            attachments.clone(),
            peer_relay,
            MESH_TIMEOUT,
            MAX_CONCURRENT_RELAYS,
        );
        gossip.control_relay().install(forwarder.clone())?;
        let reconciliation = gossip.install_reconciliation(
            identity.clone(),
            endpoint.clone(),
            MESH_QUERY_TIMEOUT,
        )?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let api_base = format!("http://{}", listener.local_addr()?);
        let http = tokio::spawn(
            axum::serve(
                listener,
                axum_support::with_connect_info(
                    ClientApi::new(capacity, forwarder, identity.clone(), endpoint.clone())
                        .with_reconciliation(reconciliation)
                        .router(),
                ),
            )
            .into_future(),
        );
        Ok(Self {
            api_base,
            attachments,
            identity,
            endpoint,
            config,
            _gossip: gossip,
            _coordinator: coordinator,
            _http: http,
            _temp: temp,
        })
    }

    pub fn record(&self) -> Result<protocol::EndpointRecord> {
        let now = crate::mesh::now_secs();
        self.identity
            .endpoint_record(&self.endpoint.addr(), now, now + 300)
    }

    pub async fn restart(self) -> Result<Self> {
        let Self {
            api_base: _,
            attachments: _,
            identity,
            endpoint,
            config,
            _gossip,
            _coordinator,
            _http,
            _temp,
        } = self;
        _http.abort();
        let _ = _http.await;
        _coordinator.shutdown().await?;
        _gossip.shutdown().await?;
        tokio::time::timeout(MESH_TIMEOUT, endpoint.close()).await?;
        let endpoint = identity
            .bind_endpoint(&config, crate::mesh::now_secs())
            .await?;
        Self::start(_temp, identity, endpoint, config).await
    }
}

pub(crate) fn config(
    members: std::collections::HashSet<iroh::EndpointId>,
    bootstraps: Vec<iroh::EndpointId>,
) -> Result<ValidatedMachineConfig> {
    Ok(ValidatedMachineConfig {
        bind_addr: "127.0.0.1:0".parse()?,
        relay_urls: vec![RELAY_URL.into()],
        relay_ca_certificates: Vec::new(),
        scheduler_members: members,
        scheduler_bootstraps: bootstraps,
        query_timeout: MESH_QUERY_TIMEOUT,
        max_pending_queries: MAX_ATTACHED_AGENTS,
        max_seen_queries: 64,
        max_attached_agents: MAX_ATTACHED_AGENTS,
        max_offers_per_query: MAX_ATTACHED_AGENTS,
        max_agent_fanout: MAX_AGENT_FANOUT,
    })
}
