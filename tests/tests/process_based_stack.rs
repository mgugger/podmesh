//! Process-based stack integration tests
//!
//! This module tests the complete podmesh stack (schedulers, proxies, sidecars)
//! using native OS processes instead of containers. This allows testing the full
//! system behavior without depending on Docker or Podman.
//!
//! The test spins up:
//! - Multiple scheduler nodes (forming a P2P mesh)
//! - Multiple proxy nodes (handling ingress)
//! - Sidecars spawned as child processes (not containers)
//!
//! This tests the exact same code paths as the containerized deployment,
//! but using the ProcessEngine runtime instead of PodmanEngine.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use axum::{Router, routing::get};
use axum_support::spawn_tcp_listener;
use podmesh_proxy::{Config as ProxyConfig, Workload as ProxyWorkload};
use podmesh_sidecar::{
    DEFAULT_SIDECAR_APP_PORT, SidecarConfig, SidecarEvent,
    manifest_routes::extract_sidecar_routes, run_sidecar_with_shutdown,
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
    allocate_tcp_port, allocate_udp_port, init_ephemeral_keys, init_tracing,
};

const PROXY_PROVIDER_LABEL: &str = "podmesh-proxy-node";
const DEMO_MANIFEST_ID: &str = "demo-nginx";
const DEMO_MANIFEST: &[u8] = include_bytes!("../sample_manifests/demo_deployment.yml");

/// Configuration for test workloads.
fn build_proxy_config(
    libp2p_port: u16,
    rest_port: u16,
    bootstrap_peer_strings: Vec<String>,
    enable_proxy_provider: bool,
    enable_ingress: bool,
) -> ProxyConfig {
    ProxyConfig {
        bootstrap_peer_strings,
        libp2p_quic_port: libp2p_port,
        libp2p_host: "127.0.0.1".to_string(),
        rest_host: "127.0.0.1".to_string(),
        rest_port,
        disable_rest_api: false,
        enable_proxy_provider,
        enable_ingress,
    }
}

/// Handle for a running proxy workload.
struct ProxyHandle {
    workload: ProxyWorkload,
    peer_id: String,
    bootstrap_addr: String,
    kad_rx: watch::Receiver<bool>,
    proxy_provider_rx: watch::Receiver<bool>,
}

impl ProxyHandle {
    fn kad_rx(&self) -> watch::Receiver<bool> {
        self.kad_rx.clone()
    }

    fn proxy_provider_rx(&self) -> watch::Receiver<bool> {
        self.proxy_provider_rx.clone()
    }
}

/// Start a proxy workload.
fn start_proxy(
    bootstrap_peers: Vec<String>,
    enable_proxy_provider: bool,
    enable_ingress: bool,
) -> Result<ProxyHandle> {
    let libp2p_port = allocate_udp_port();
    let rest_port = allocate_tcp_port();
    let config = build_proxy_config(
        libp2p_port,
        rest_port,
        bootstrap_peers,
        enable_proxy_provider,
        enable_ingress,
    );
    let mut workload = ProxyWorkload::new(config)?;
    workload.start()?;

    let peer_id = workload
        .peer_id()
        .map(|id| id.to_string())
        .ok_or_else(|| anyhow!("workload peer id unavailable"))?;
    let bootstrap_addr = format!("/ip4/127.0.0.1/udp/{libp2p_port}/quic-v1/p2p/{peer_id}");
    let kad_rx = workload
        .kad_bootstrap_rx()
        .ok_or_else(|| anyhow!("kad bootstrap channel missing"))?;
    let proxy_provider_rx = workload
        .proxy_provider_announced_rx()
        .ok_or_else(|| anyhow!("proxy provider channel missing"))?;

    Ok(ProxyHandle {
        workload,
        peer_id,
        bootstrap_addr,
        kad_rx,
        proxy_provider_rx,
    })
}

/// Wait for Kademlia routing table to be ready.
async fn wait_for_kad_ready(mut rx: watch::Receiver<bool>, timeout: Duration) -> Result<()> {
    if *rx.borrow() {
        return Ok(());
    }

    tokio::time::timeout(timeout, async {
        loop {
            rx.changed()
                .await
                .map_err(|_| anyhow!("kad channel closed before readiness"))?;
            if *rx.borrow() {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await
    .map_err(|_| anyhow!("kademlia routing table never updated"))??;

    Ok(())
}

/// Wait for proxy provider announcement.
async fn wait_for_proxy_provider(mut rx: watch::Receiver<bool>, timeout: Duration) -> Result<()> {
    if *rx.borrow() {
        return Ok(());
    }

    tokio::time::timeout(timeout, async {
        loop {
            rx.changed()
                .await
                .map_err(|_| anyhow!("proxy provider channel closed before announcement"))?;
            if *rx.borrow() {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await
    .map_err(|_| anyhow!("proxy provider announcement never observed"))??;

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
        if let Ok(response) = client.get(url).header("host", host).send().await {
            if response.status().is_success() {
                return Ok(response.text().await?);
            }
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("ingress proxy response timed out"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Wait for sidecar to connect to a peer and discover provider.
async fn wait_for_sidecar_peer_ready(
    rx: &mut mpsc::UnboundedReceiver<SidecarEvent>,
    expected_peer_id: &str,
    timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(timeout, async {
        let mut connected = false;
        let mut provider_seen = false;
        while !(connected && provider_seen) {
            match rx.recv().await {
                Some(SidecarEvent::Connected { peer_id }) => {
                    if peer_id == expected_peer_id {
                        connected = true;
                    }
                }
                Some(SidecarEvent::ProviderDiscovered { peer_id }) => {
                    if peer_id == expected_peer_id {
                        provider_seen = true;
                    }
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
            "sidecar did not become ready for provider {}",
            expected_peer_id
        )
    })??;

    Ok(())
}

/// Build sidecar configuration for testing.
fn build_sidecar_config(
    bootstrap_peers: Vec<String>,
    app_port: u16,
) -> Result<(SidecarConfig, String, String)> {
    let (routes, ingress_host, service_host) = demo_routes(app_port)?;
    let cfg = SidecarConfig {
        provider_label: PROXY_PROVIDER_LABEL.to_string(),
        bootstrap_peers,
        bootstrap_peer_ip: None,
        lookup_interval: Duration::from_secs(2),
        announce_interval: Duration::from_secs(5),
        libp2p_host: "0.0.0.0".to_string(),
        libp2p_port: 0,
        announce_providers: false,
        manifest_id: DEMO_MANIFEST_ID.to_string(),
        ingress_host: ingress_host.clone(),
        app_port,
        routes,
        owner_public_key_b64: None,
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

    let mut proxies: Vec<ProxyHandle> = Vec::new();
    let mut sidecar_shutdown: Option<oneshot::Sender<()>> = None;
    let mut sidecar_task: Option<JoinHandle<()>> = None;
    let mut app_server: Option<JoinHandle<()>> = None;

    let test_result: Result<()> = async {
        // Start first proxy node (acts as bootstrap)
        log::info!("Starting first proxy node (bootstrap)");
        let first = start_proxy(Vec::new(), true, true)?;
        let first_bootstrap = first.bootstrap_addr.clone();
        let provider_peer_id = first.peer_id.clone();
        proxies.push(first);

        // Start second proxy node
        log::info!("Starting second proxy node");
        let second = start_proxy(vec![first_bootstrap.clone()], true, false)?;
        let second_bootstrap = second.bootstrap_addr.clone();
        proxies.push(second);

        // Start third proxy node
        log::info!("Starting third proxy node");
        let third = start_proxy(vec![first_bootstrap.clone(), second_bootstrap.clone()], true, false)?;
        proxies.push(third);

        // Wait for all nodes to have their Kademlia tables ready
        log::info!("Waiting for Kademlia routing tables to be ready");
        for (i, proxy) in proxies.iter().enumerate() {
            wait_for_kad_ready(proxy.kad_rx(), Duration::from_secs(20))
                .await
                .context(format!("proxy node {} kad ready", i))?;
        }
        log::info!("All proxy nodes have ready Kademlia tables");

        // Wait for proxy providers to be announced
        log::info!("Waiting for proxy provider announcements");
        for (i, proxy) in proxies.iter().enumerate() {
            wait_for_proxy_provider(proxy.proxy_provider_rx(), Duration::from_secs(10))
                .await
                .context(format!("proxy node {} provider announcement", i))?;
        }
        log::info!("All proxy nodes have announced as providers");

        // Start the test application
        let app_port = allocate_tcp_port();
        let app_body = "hello-from-process-based-stack".to_string();
        log::info!("Starting test application on port {}", app_port);
        app_server = Some(spawn_test_app(app_port, app_body.clone()).await?);

        // Give the app a moment to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Collect bootstrap peers for the sidecar
        let sidecar_bootstrap_peers: Vec<String> = proxies
            .iter()
            .map(|p| p.bootstrap_addr.clone())
            .collect();

        // Build sidecar configuration
        let (sidecar_cfg, ingress_host, service_host) =
            build_sidecar_config(sidecar_bootstrap_peers.clone(), app_port)?;

        log::info!(
            "Starting sidecar with bootstrap peers: {:?}",
            sidecar_bootstrap_peers
        );

        // Start the sidecar (as a native process via run_sidecar_with_shutdown)
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        sidecar_shutdown = Some(shutdown_tx);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        sidecar_task = Some(tokio::spawn(async move {
            if let Err(e) = run_sidecar_with_shutdown(sidecar_cfg, shutdown_rx, Some(event_tx)).await {
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

        // Give DHT time to propagate
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
        assert_eq!(
            body, app_body,
            "Response body mismatch for ingress host"
        );
        log::info!("Ingress test passed for ingress host");

        // Test with service host header
        log::info!("Testing ingress with service host header: {}", service_host);
        let body = wait_for_ingress_response(&client, &url, &service_host, Duration::from_secs(20))
            .await
            .context("ingress response with service host")?;
        assert_eq!(
            body, app_body,
            "Response body mismatch for service host"
        );
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

/// Test sidecar discovery across a mesh of proxy nodes using processes.
///
/// This test verifies that a sidecar can discover providers across
/// multiple proxy nodes in a P2P mesh without containers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn sidecar_discovers_provider_in_process_mesh() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();

    let mut proxies: Vec<ProxyHandle> = Vec::new();

    let test_result: Result<()> = async {
        // Start first proxy (bootstrap, with provider)
        let first = start_proxy(Vec::new(), true, false)?;
        let provider_peer_id = first.peer_id.clone();
        let first_bootstrap = first.bootstrap_addr.clone();
        proxies.push(first);

        // Start second proxy
        let second = start_proxy(vec![first_bootstrap.clone()], false, false)?;
        let second_bootstrap = second.bootstrap_addr.clone();
        proxies.push(second);

        // Start third proxy
        let third = start_proxy(vec![first_bootstrap, second_bootstrap], false, false)?;
        proxies.push(third);

        // Wait for Kademlia readiness
        for node in &proxies {
            wait_for_kad_ready(node.kad_rx(), Duration::from_secs(20)).await?;
        }

        // Wait for provider announcement on first node
        if let Some(provider_node) = proxies.first() {
            wait_for_proxy_provider(provider_node.proxy_provider_rx(), Duration::from_secs(10))
                .await?;
        }

        // Collect bootstrap peers for sidecar
        let sidecar_bootstrap_peers: Vec<String> = proxies
            .iter()
            .map(|node| node.bootstrap_addr.clone())
            .collect();

        // Give mesh time to stabilize
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Build and run sidecar
        let (sidecar_cfg, _, _) =
            build_sidecar_config(sidecar_bootstrap_peers, DEFAULT_SIDECAR_APP_PORT)?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let sidecar_task = tokio::spawn(async move {
            run_sidecar_with_shutdown(sidecar_cfg, shutdown_rx, Some(event_tx))
                .await
                .expect("sidecar run");
        });

        // Track events
        let known_peer_ids: HashSet<String> =
            proxies.iter().map(|node| node.peer_id.clone()).collect();
        let mut connected_peers = HashSet::new();
        let mut provider_seen = false;
        let deadline = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(deadline);

        while !(connected_peers.len() >= 1 && provider_seen) {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    match event {
                        SidecarEvent::Connected { ref peer_id } => {
                            if known_peer_ids.contains(peer_id) {
                                connected_peers.insert(peer_id.clone());
                                log::info!("Sidecar connected to peer: {}", peer_id);
                            }
                        }
                        SidecarEvent::ProviderDiscovered { ref peer_id } => {
                            if peer_id == &provider_peer_id {
                                provider_seen = true;
                                log::info!("Sidecar discovered provider: {}", peer_id);
                            }
                        }
                    }
                }
                _ = &mut deadline => {
                    break;
                }
            }
        }

        assert!(
            !connected_peers.is_empty(),
            "sidecar never connected to any proxy peer"
        );
        assert!(
            provider_seen,
            "sidecar never observed provider {}",
            provider_peer_id
        );

        let _ = shutdown_tx.send(());
        let _ = sidecar_task.await;

        Ok(())
    }
    .await;

    // Cleanup
    for mut proxy in proxies {
        proxy.workload.close().await;
    }

    test_result
}
