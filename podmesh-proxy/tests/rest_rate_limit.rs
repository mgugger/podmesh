//! The proxy REST API is unauthenticated by design, so its bound is the
//! per-peer rate limit. These tests drive real HTTP against a bound listener,
//! which is the only way to know the middleware is actually reached: a limiter
//! that exists but is not wired into the router looks identical from the unit
//! tests.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use podmesh_proxy::proxy_grants::ProxyGrantStore;
use podmesh_proxy::restapi::{RestServerOptions, WorkloadRelayBootstrap, spawn_rest_server};
use tokio::sync::watch;

/// Requests per minute used by the throttled server under test. Small enough
/// that the budget is exhausted quickly, large enough to prove several requests
/// do get through first.
const TEST_RATE_LIMIT: u32 = 5;

fn endpoint_record() -> Result<Arc<RwLock<protocol::EndpointRecord>>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let (public, private) = crypto::generate_signing_keypair();
    let record = protocol::EndpointRecord {
        version: protocol::ENDPOINT_RECORD_VERSION,
        endpoint_id: iroh::SecretKey::generate().public().as_bytes().to_vec(),
        relay_url: None,
        direct_addresses: vec!["127.0.0.1:4002".into()],
        signing_pubkey: String::new(),
        issued_at_secs: now,
        expires_at_secs: now + 600,
        signature: String::new(),
    }
    .sign(&public, &private, now)?;
    Ok(Arc::new(RwLock::new(record)))
}

/// Start a REST API on a free port and return its base URL.
///
/// The port has to be released before the server can bind it, so another
/// process can take it in between; the whole start is retried rather than
/// failing the test on a lost race.
async fn start_rest_api(rate_limit_per_minute: u32) -> Result<String> {
    for _ in 0..16 {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))?
            .local_addr()?
            .port();
        let (_peer_tx, peer_rx) = watch::channel(Vec::new());
        spawn_rest_server(RestServerOptions {
            host: "127.0.0.1".into(),
            port,
            peer_rx,
            local_peer_id: "peer".into(),
            endpoint_record: endpoint_record()?,
            grant_store: ProxyGrantStore::new(),
            relay_bootstrap: None::<WorkloadRelayBootstrap>,
            rate_limit_per_minute,
        })?;

        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        for _ in 0..40 {
            if client
                .get(format!("{base}/healthz"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return Ok(base);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    anyhow::bail!("rest api never became reachable")
}

#[tokio::test]
async fn a_caller_exceeding_its_budget_is_refused() -> Result<()> {
    let base = start_rest_api(TEST_RATE_LIMIT).await?;
    let client = reqwest::Client::new();

    // The budget is spent on real API calls, not on health probes.
    for attempt in 0..TEST_RATE_LIMIT {
        let status = client
            .get(format!("{base}/api/v1/peer_id"))
            .send()
            .await
            .context("send request")?
            .status();
        anyhow::ensure!(
            status.is_success(),
            "request {attempt} within the budget returned {status}"
        );
    }

    let status = client
        .get(format!("{base}/api/v1/peer_id"))
        .send()
        .await
        .context("send request")?
        .status();
    anyhow::ensure!(
        status == reqwest::StatusCode::TOO_MANY_REQUESTS,
        "a caller past its budget must be refused, got {status}"
    );
    Ok(())
}

/// The grant endpoint is the one an attacker would use to fill the bounded
/// grant store, so it has to be behind the limiter rather than beside it.
#[tokio::test]
async fn the_unauthenticated_grant_endpoint_is_throttled() -> Result<()> {
    let base = start_rest_api(TEST_RATE_LIMIT).await?;
    let client = reqwest::Client::new();
    let body = serde_json::json!({ "owner_pubkey_b64": "", "grant_b64": "" });

    let mut refused = false;
    for _ in 0..(TEST_RATE_LIMIT + 2) {
        let status = client
            .post(format!("{base}/api/v1/proxy_grant"))
            .json(&body)
            .send()
            .await
            .context("send grant")?
            .status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            refused = true;
            break;
        }
    }
    anyhow::ensure!(
        refused,
        "posting grants past the budget must be refused rather than always evaluated"
    );
    Ok(())
}

/// Liveness must answer even when a client has burned its whole budget,
/// otherwise a noisy neighbour takes the proxy out of its orchestrator's pool.
#[tokio::test]
async fn health_is_never_throttled() -> Result<()> {
    let base = start_rest_api(TEST_RATE_LIMIT).await?;
    let client = reqwest::Client::new();

    for _ in 0..(TEST_RATE_LIMIT + 5) {
        let _ = client.get(format!("{base}/api/v1/peer_id")).send().await;
    }
    anyhow::ensure!(
        client
            .get(format!("{base}/api/v1/peer_id"))
            .send()
            .await?
            .status()
            == reqwest::StatusCode::TOO_MANY_REQUESTS,
        "precondition: the budget should be exhausted"
    );

    for attempt in 0..10 {
        let status = client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .context("send health probe")?
            .status();
        anyhow::ensure!(
            status.is_success(),
            "health probe {attempt} was throttled with {status}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_zero_limit_disables_throttling() -> Result<()> {
    let base = start_rest_api(0).await?;
    let client = reqwest::Client::new();
    for attempt in 0..50 {
        let status = client
            .get(format!("{base}/api/v1/peer_id"))
            .send()
            .await
            .context("send request")?
            .status();
        anyhow::ensure!(
            status.is_success(),
            "request {attempt} was throttled despite the limiter being disabled: {status}"
        );
    }
    Ok(())
}
