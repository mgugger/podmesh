//! Process-based workload-plane integration tests
//!
//! This module exercises the workload traffic plane — proxy ingress, proxy
//! egress, and sidecar route matching — using native OS processes instead of
//! containers, so it can run without Podman.
//!
//! Scope and non-scope:
//! - In scope: proxy and sidecar wired over Iroh, owner-signed Biscuit grant handshake,
//!   ingress routing to an in-process application, and egress tunnelling.
//! - Not in scope: the machine plane. Scheduler placement and agent deployment
//!   are covered by `podmesh-agent/tests/scheduler_relay.rs`, which runs a real
//!   scheduler and a real agent and drives the same HTTP client API `podctl`
//!   uses. Full multi-replica deployment over Podman is covered by
//!   `complete_rootless_stack.rs` behind the `podman-tests` feature.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use axum::{Router, routing::get};
use axum_support::spawn_tcp_listener;
use podmesh_proxy::{Config as ProxyConfig, Workload as ProxyWorkload};
use podmesh_sidecar::{
    SidecarConfig, SidecarEvent, manifest_routes::extract_sidecar_routes, run_sidecar_with_shutdown,
};
use protocol::machine::{SidecarRouteKind, SidecarRouteSpec};
use reqwest::Client;
use serial_test::serial;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

use podmesh_integration_tests::support::{
    allocate_tcp_port, allocate_udp_port, fresh_tenant_owner, init_ephemeral_keys, init_tracing,
    provision_proxy_cert,
};

const DEMO_MANIFEST_ID: &str = "demo-nginx";
const DEMO_MANIFEST: &[u8] = include_bytes!("../sample_manifests/demo_deployment.yml");

/// Configuration for test workloads.
fn build_proxy_config(
    iroh_port: u16,
    rest_port: u16,
    proxy_endpoints: Vec<protocol::EndpointRecord>,
    enable_ingress: bool,
) -> ProxyConfig {
    ProxyConfig {
        proxy_endpoints,
        identity: podmesh_proxy::IdentitySource::ephemeral(),
        iroh_bind_addr: format!("127.0.0.1:{iroh_port}").parse().unwrap(),
        workload_relay: None,
        workload_relay_certificate_der: Vec::new(),
        publish_relay_bootstrap: false,
        rest_host: "127.0.0.1".to_string(),
        rest_port,
        disable_rest_api: false,
        enable_ingress,
        owner_pubkey: None,
    }
}

/// Handle for a running proxy workload.
struct ProxyHandle {
    workload: ProxyWorkload,
    peer_id: String,
    endpoint_record: protocol::EndpointRecord,
    rest_port: u16,
    network_ready_rx: watch::Receiver<bool>,
}

impl ProxyHandle {
    fn network_ready_rx(&self) -> watch::Receiver<bool> {
        self.network_ready_rx.clone()
    }
}

/// Start a proxy workload.
async fn start_proxy(
    bootstrap_peers: Vec<protocol::EndpointRecord>,
    enable_ingress: bool,
) -> Result<ProxyHandle> {
    let iroh_port = allocate_udp_port();
    let rest_port = allocate_tcp_port();
    let config = build_proxy_config(iroh_port, rest_port, bootstrap_peers, enable_ingress);
    let mut workload = ProxyWorkload::new(config)?;
    workload.start().await?;

    let peer_id = workload
        .peer_id()
        .map(|id| id.to_string())
        .ok_or_else(|| anyhow!("workload peer id unavailable"))?;
    let endpoint_record = workload
        .endpoint_record()
        .ok_or_else(|| anyhow!("workload EndpointRecord unavailable"))?;
    let network_ready_rx = workload
        .network_ready_rx()
        .ok_or_else(|| anyhow!("network readiness channel missing"))?;

    Ok(ProxyHandle {
        workload,
        peer_id,
        endpoint_record,
        rest_port,
        network_ready_rx,
    })
}

/// Wait for the proxy listener to be ready.
async fn wait_for_network_ready(mut rx: watch::Receiver<bool>, timeout: Duration) -> Result<()> {
    if *rx.borrow() {
        return Ok(());
    }

    tokio::time::timeout(timeout, async {
        loop {
            rx.changed()
                .await
                .map_err(|_| anyhow!("network readiness channel closed"))?;
            if *rx.borrow() {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await
    .map_err(|_| anyhow!("proxy listener never became ready"))??;

    Ok(())
}

/// Spawn a test HTTP application.
async fn spawn_test_app(port: u16, response_body: String) -> Result<JoinHandle<()>> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let router = Router::new().route(
        "/hello",
        get({
            let response_body = response_body.clone();
            move || {
                let body = response_body.clone();
                async move { body }
            }
        }),
    );
    Ok(spawn_tcp_listener(listener, router, "process-test-app"))
}

/// Wait for ingress to return a successful response.
async fn wait_for_ingress_response(
    client: &Client,
    url: &str,
    host: &str,
    timeout: Duration,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(response) = client.get(url).header("host", host).send().await
            && response.status().is_success()
        {
            return Ok(response.text().await?);
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("ingress proxy response timed out"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Wait for the sidecar to connect to and discover the expected proxy.
async fn wait_for_sidecar_peer_ready(
    rx: &mut mpsc::UnboundedReceiver<SidecarEvent>,
    expected_peer_id: &str,
    timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(timeout, async {
        let mut connected = false;
        let mut proxy_seen = false;
        while !(connected && proxy_seen) {
            match rx.recv().await {
                Some(SidecarEvent::Connected { peer_id }) => {
                    if peer_id == expected_peer_id {
                        connected = true;
                    }
                }
                Some(SidecarEvent::ProxyPeerDiscovered { peer_id }) => {
                    if peer_id == expected_peer_id {
                        proxy_seen = true;
                    }
                }
                Some(SidecarEvent::EgressTunnelEstablished { .. })
                | Some(SidecarEvent::EgressTunnelFailed { .. }) => {
                    // Ignored in this helper
                }
                None => {
                    return Err(anyhow!("sidecar event channel closed before readiness"));
                }
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| {
        anyhow!(
            "sidecar did not become ready for proxy {}",
            expected_peer_id
        )
    })??;

    Ok(())
}

/// Build sidecar configuration for testing.
fn build_sidecar_config(
    proxy_endpoints: Vec<protocol::EndpointRecord>,
    app_port: u16,
) -> Result<(SidecarConfig, String, String)> {
    let (routes, ingress_host, service_host) = demo_routes(app_port)?;
    let cfg = SidecarConfig {
        identity: podmesh_sidecar::IdentitySource::ephemeral(),
        proxy_endpoints,
        workload_relay_auth_token: None,
        workload_relay_ca_certificates: Vec::new(),
        lookup_interval: Duration::from_secs(2),
        iroh_bind_addr: "127.0.0.1:0".parse()?,
        manifest_id: DEMO_MANIFEST_ID.to_string(),
        ingress_host: ingress_host.clone(),
        app_port,
        routes,
        owner_public_key_b64: None,
        enable_egress: false,
        skip_egress_nft: false,
        http_proxy_port: None,
    };
    Ok((cfg, ingress_host, service_host))
}

/// Extract routes from the demo manifest.
fn demo_routes(app_port: u16) -> Result<(Vec<SidecarRouteSpec>, String, String)> {
    let extraction = extract_sidecar_routes(DEMO_MANIFEST, DEMO_MANIFEST_ID)?;
    let mut routes = extraction.routes;
    for route in routes.iter_mut() {
        route.target_port = app_port;
    }

    let ingress_host = routes
        .iter()
        .find(|route| matches!(route.source, SidecarRouteKind::Ingress))
        .map(|route| route.host.clone())
        .ok_or_else(|| anyhow!("demo manifest missing ingress route"))?;
    let service_host = routes
        .iter()
        .find(|route| matches!(route.source, SidecarRouteKind::Service))
        .map(|route| route.host.clone())
        .ok_or_else(|| anyhow!("demo manifest missing service route"))?;

    Ok((routes, ingress_host, service_host))
}

/// Test the complete process-based stack with multiple proxy nodes and sidecars.
///
/// This test:
/// 1. Starts 3 proxy nodes forming a P2P mesh
/// 2. Spawns a sidecar as a native process (not container)
/// 3. Starts a test HTTP application
/// 4. Verifies that ingress routes requests through the sidecar to the app
///
/// This exercises the same code paths as the containerized deployment but
/// without requiring Docker or Podman.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn process_based_stack_with_multiple_nodes() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();
    let (owner_b64, owner_sk, owner_pk) = fresh_tenant_owner();

    let mut proxies: Vec<ProxyHandle> = Vec::new();
    let mut sidecar_shutdown: Option<oneshot::Sender<()>> = None;
    let mut sidecar_task: Option<JoinHandle<()>> = None;
    let mut app_server: Option<JoinHandle<()>> = None;

    let test_result: Result<()> = async {
        // Start first proxy node (acts as bootstrap)
        log::info!("Starting first proxy node (bootstrap)");
        let first = start_proxy(Vec::new(), true).await?;
        let first_bootstrap = first.endpoint_record.clone();
        let provider_peer_id = first.peer_id.clone();
        proxies.push(first);

        // Start second proxy node
        log::info!("Starting second proxy node");
        let second = start_proxy(vec![first_bootstrap.clone()], false).await?;
        let second_bootstrap = second.endpoint_record.clone();
        proxies.push(second);

        // Start third proxy node
        log::info!("Starting third proxy node");
        let third = start_proxy(
            vec![first_bootstrap.clone(), second_bootstrap.clone()],
            false,
        )
        .await?;
        proxies.push(third);

        log::info!("Waiting for proxy listeners to be ready");
        for (i, proxy) in proxies.iter().enumerate() {
            wait_for_network_ready(proxy.network_ready_rx(), Duration::from_secs(20))
                .await
                .context(format!("proxy node {} network ready", i))?;
        }
        log::info!("All proxy listeners are ready");
        for proxy in &proxies {
            provision_proxy_cert(
                proxy.rest_port,
                &owner_pk,
                &owner_sk,
                Duration::from_secs(10),
            )
            .await?;
        }

        // Start the test application
        let app_port = allocate_tcp_port();
        let app_body = "hello-from-process-based-stack".to_string();
        log::info!("Starting test application on port {}", app_port);
        app_server = Some(spawn_test_app(app_port, app_body.clone()).await?);

        // Give the app a moment to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Collect bootstrap peers for the sidecar
        let sidecar_bootstrap_peers = proxies
            .iter()
            .map(|proxy| proxy.endpoint_record.clone())
            .collect::<Vec<_>>();

        // Build sidecar configuration
        let (mut sidecar_cfg, ingress_host, service_host) =
            build_sidecar_config(sidecar_bootstrap_peers.clone(), app_port)?;
        sidecar_cfg.owner_public_key_b64 = Some(owner_b64.clone());

        log::info!(
            "Starting sidecar with bootstrap peers: {:?}",
            sidecar_bootstrap_peers
        );

        // Start the sidecar (as a native process via run_sidecar_with_shutdown)
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        sidecar_shutdown = Some(shutdown_tx);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        sidecar_task = Some(tokio::spawn(async move {
            if let Err(e) =
                run_sidecar_with_shutdown(sidecar_cfg, shutdown_rx, Some(event_tx)).await
            {
                log::error!("Sidecar exited with error: {:?}", e);
            }
        }));

        // Wait for sidecar to connect and discover the provider
        log::info!(
            "Waiting for sidecar to connect to provider {}",
            provider_peer_id
        );
        wait_for_sidecar_peer_ready(&mut event_rx, &provider_peer_id, Duration::from_secs(30))
            .await
            .context("sidecar peer readiness")?;

        log::info!("Sidecar connected and discovered provider");

        // Give route registrations time to settle.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Test ingress routing via the first proxy (which has ingress enabled)
        let ingress_addr = proxies
            .first()
            .and_then(|p| p.workload.ingress_address())
            .ok_or_else(|| anyhow!("ingress listen address unavailable"))?;

        let client = Client::new();
        let url = format!("http://{ingress_addr}/hello");

        // Test with ingress host header
        log::info!("Testing ingress with host header: {}", ingress_host);
        let body = wait_for_ingress_response(&client, &url, &ingress_host, Duration::from_secs(20))
            .await
            .context("ingress response with ingress host")?;
        assert_eq!(body, app_body, "Response body mismatch for ingress host");
        log::info!("Ingress test passed for ingress host");

        // Test with service host header
        log::info!("Testing ingress with service host header: {}", service_host);
        let body = wait_for_ingress_response(&client, &url, &service_host, Duration::from_secs(20))
            .await
            .context("ingress response with service host")?;
        assert_eq!(body, app_body, "Response body mismatch for service host");
        log::info!("Ingress test passed for service host");

        log::info!("All process-based stack tests passed!");

        Ok(())
    }
    .await;

    // Cleanup
    log::info!("Cleaning up test resources");

    if let Some(tx) = sidecar_shutdown.take() {
        let _ = tx.send(());
    }
    if let Some(task) = sidecar_task.take() {
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    if let Some(server) = app_server.take() {
        server.abort();
        let _ = server.await;
    }
    for mut proxy in proxies {
        proxy.workload.close().await;
    }

    test_result
}
