//! The scheduler client API is unauthenticated on purpose: the mesh is open and
//! anyone may place a workload. Its bound is therefore the per-peer rate limit,
//! and these tests drive real HTTP to prove the limiter is on the router rather
//! than merely present in the workspace.

use iroh::address_lookup::memory::MemoryLookup;
mod common;

use std::time::Duration;

use anyhow::{Context, Result};
use podmesh_scheduler::{
    clientapi::ClientApi,
    machine::{
        AgentControlForwarder, AttachmentManager, CapacityCoordinator, LocationRegistry,
        MemberIssuers, PeerControlRelay, PlacementHandler, QueryManager, SchedulerGossip,
        SchedulerGossipServices, SchedulerIdentity,
    },
};
use std::collections::HashSet;

/// Small enough to exhaust quickly, large enough to show requests succeed first.
const TEST_RATE_LIMIT: u32 = 4;
const MAX_ATTACHED: usize = 8;

/// Start a scheduler client API on a free port and return its base URL.
///
/// The listener is bound here and handed to the server, so there is no window
/// in which another process could take the port.
async fn start_client_api(
    rate_limit_per_minute: u32,
) -> Result<(String, podmesh_metrics::Metrics)> {
    let temp = tempfile::tempdir()?;
    let identity = SchedulerIdentity::load(temp.path())?;
    let config = common::config(HashSet::from([identity.endpoint_id()]), Vec::new());
    let endpoint = identity.bind_endpoint(&config, now_secs()).await?;

    let attachments = AttachmentManager::new(MAX_ATTACHED, MAX_ATTACHED, common::TEST_TIMEOUT);
    let queries = QueryManager::new(MAX_ATTACHED, MAX_ATTACHED, Duration::from_millis(50));
    let gossip = SchedulerGossip::start(
        SchedulerGossipServices {
            endpoint: endpoint.clone(),
            attachments: attachments.handler(),
            offers: queries.offer_handler(),
            placement: PlacementHandler::new(MAX_ATTACHED, common::TEST_TIMEOUT),
            locations: LocationRegistry::new(),
            member_issuers: MemberIssuers::new(),
            lookup: MemoryLookup::new(),
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
    let forwarder = AgentControlForwarder::new(
        endpoint.clone(),
        attachments,
        PeerControlRelay::new(
            endpoint.clone(),
            gossip.members(),
            LocationRegistry::new(),
            identity.clone(),
            common::TEST_TIMEOUT,
        ),
        common::TEST_TIMEOUT,
        MAX_ATTACHED,
    );

    let metrics = podmesh_metrics::Metrics::registered(podmesh_metrics::ComponentName::Scheduler);
    let router = ClientApi::new(capacity, forwarder, identity, endpoint)
        .with_metrics(metrics.clone())
        .with_rate_limit(rate_limit_per_minute)
        .router();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move {
        // The scheduler is kept alive by moving its components into the task
        // that serves the API.
        let _keep_alive = (gossip, coordinator, temp);
        let _ = axum::serve(listener, axum_support::with_connect_info(router)).await;
    });
    Ok((base, metrics))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// One cheap HTTP call fans a signed query out to every agent in the mesh, so
/// selection is the request that most needs a ceiling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selection_is_throttled_per_peer() -> Result<()> {
    let (base, metrics) = start_client_api(TEST_RATE_LIMIT).await?;
    let client = reqwest::Client::new();

    let mut refused = false;
    for _ in 0..(TEST_RATE_LIMIT + 4) {
        let status = client
            .get(format!("{base}/api/v1/agents/select"))
            .query(&[
                ("cpu_milli", "1"),
                ("memory_bytes", "1"),
                ("storage_bytes", "1"),
            ])
            .send()
            .await
            .context("send selection request")?
            .status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            refused = true;
            break;
        }
    }
    anyhow::ensure!(
        refused,
        "a caller past its budget must be refused rather than fanning out again"
    );
    let snapshot = metrics.snapshot().context("scheduler metrics snapshot")?;
    anyhow::ensure!(
        snapshot.events().any(|(key, count)| {
            key.event() == podmesh_metrics::EventName::RateLimitRefusal && *count == 1
        }),
        "the rate-limit decision owner must emit one bounded refusal event"
    );
    Ok(())
}

/// A relay call opens a QUIC connection towards an agent, so it is throttled on
/// the same budget — and refused before the body is even considered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_relay_is_throttled_per_peer() -> Result<()> {
    let (base, _) = start_client_api(TEST_RATE_LIMIT).await?;
    let client = reqwest::Client::new();
    let agent = "a".repeat(64);

    let mut refused = false;
    for _ in 0..(TEST_RATE_LIMIT + 4) {
        let status = client
            .post(format!("{base}/api/v1/agents/{agent}/command"))
            .body(vec![0u8; 32])
            .send()
            .await
            .context("send relay request")?
            .status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            refused = true;
            break;
        }
    }
    anyhow::ensure!(refused, "relay calls must share the per-peer budget");
    Ok(())
}

/// An orchestrator polling liveness must not be throttled by unrelated client
/// traffic, or a busy scheduler gets pulled out of its pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_and_ready_are_never_throttled() -> Result<()> {
    let (base, _) = start_client_api(TEST_RATE_LIMIT).await?;
    let client = reqwest::Client::new();

    for _ in 0..(TEST_RATE_LIMIT + 6) {
        let _ = client
            .get(format!("{base}/api/v1/agents/select"))
            .query(&[
                ("cpu_milli", "1"),
                ("memory_bytes", "1"),
                ("storage_bytes", "1"),
            ])
            .send()
            .await;
    }

    for path in ["health", "ready"] {
        for attempt in 0..8 {
            let status = client
                .get(format!("{base}/{path}"))
                .send()
                .await
                .context("send probe")?
                .status();
            anyhow::ensure!(
                status.is_success(),
                "{path} probe {attempt} was throttled with {status}"
            );
        }
    }
    Ok(())
}
