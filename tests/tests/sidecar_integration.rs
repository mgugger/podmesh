use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use axum::{Router, routing::get};
use axum_support::spawn_tcp_listener;
use podmesh_proxy::{Config, Workload};
use podmesh_sidecar::{
    DEFAULT_SIDECAR_APP_PORT, SidecarConfig, SidecarEvent, manifest_routes::extract_sidecar_routes,
    run_sidecar_with_shutdown,
};
use protocol::machine::{SidecarRouteKind, SidecarRouteSpec};
use reqwest::Client;
use serial_test::serial;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

use podmesh_integration_tests::support::{allocate_tcp_port, allocate_udp_port, init_ephemeral_keys, init_tracing};

const SIDECAR_PROVIDER_LABEL: &str = "podmesh-proxy-node";
const DEMO_MANIFEST_ID: &str = "demo-nginx";
const DEMO_MANIFEST: &[u8] = include_bytes!("../sample_manifests/demo_deployment.yml");

fn build_workload_config(
    libp2p_port: u16,
    rest_port: u16,
    bootstrap_peer_strings: Vec<String>,
    enable_proxy_provider: bool,
    enable_ingress: bool,
) -> Config {
    Config {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn sidecar_discovers_workload_provider() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();
    let mut workloads = Vec::new();

    let test_result: Result<()> = async {
        let first = start_workload(Vec::new(), true, false)?;
        let provider_peer_id = first.peer_id.clone();
        let first_bootstrap = first.bootstrap_addr.clone();
        workloads.push(first);

        let second = start_workload(vec![first_bootstrap.clone()], false, false)?;
        let second_bootstrap = second.bootstrap_addr.clone();
        workloads.push(second);

        let third = start_workload(vec![first_bootstrap, second_bootstrap], false, false)?;
        workloads.push(third);

        for node in &workloads {
            wait_for_kad_ready(node.kad_rx(), Duration::from_secs(20)).await?;
        }

        if let Some(provider_node) = workloads.first() {
            wait_for_proxy_provider(provider_node.proxy_provider_rx(), Duration::from_secs(10))
                .await?;
        }

        let sidecar_bootstrap_peers: Vec<String> = workloads
            .iter()
            .map(|node| node.bootstrap_addr.clone())
            .collect();

        tokio::time::sleep(Duration::from_secs(1)).await;

        let (sidecar_cfg, _, _) =
            build_sidecar_config(sidecar_bootstrap_peers, DEFAULT_SIDECAR_APP_PORT)?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let sidecar_task = tokio::spawn(async move {
            run_sidecar_with_shutdown(sidecar_cfg, shutdown_rx, Some(event_tx))
                .await
                .expect("sidecar run");
        });

        let known_peer_ids: HashSet<String> =
            workloads.iter().map(|node| node.peer_id.clone()).collect();
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
                            }
                        }
                        SidecarEvent::ProviderDiscovered { ref peer_id } => {
                            if peer_id == &provider_peer_id {
                                provider_seen = true;
                            }
                        }
                        SidecarEvent::ProxyPeerDiscovered { .. } 
                        | SidecarEvent::EgressTunnelEstablished { .. }
                        | SidecarEvent::EgressTunnelFailed { .. } => {
                            // Ignored in this test
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
            "sidecar never connected to any workload peer"
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

    for node in workloads.iter_mut() {
        node.workload.close().await;
    }

    test_result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ingress_proxies_requests_via_sidecar() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();
    let mut handle = start_workload(Vec::new(), true, true)?;
    let mut sidecar_shutdown: Option<oneshot::Sender<()>> = None;
    let mut sidecar_task: Option<JoinHandle<()>> = None;
    let mut app_server: Option<JoinHandle<()>> = None;
    let test_result: Result<()> = async {
        wait_for_kad_ready(handle.kad_rx(), Duration::from_secs(10)).await?;
        wait_for_proxy_provider(handle.proxy_provider_rx(), Duration::from_secs(10)).await?;

        let app_port = allocate_tcp_port();

        let app_body = "hello-from-proxied-app".to_string();
        app_server = Some(spawn_test_app(app_port, app_body.clone()).await?);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        sidecar_shutdown = Some(shutdown_tx);
        let (sidecar_cfg, ingress_host, service_host) =
            build_sidecar_config(vec![handle.bootstrap_addr.clone()], app_port)?;
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let provider_peer_id = handle.peer_id.clone();

        sidecar_task = Some(tokio::spawn(async move {
            run_sidecar_with_shutdown(sidecar_cfg, shutdown_rx, Some(event_tx))
                .await
                .expect("sidecar run");
        }));

        wait_for_sidecar_peer_ready(&mut event_rx, &provider_peer_id, Duration::from_secs(20))
            .await?;

        tokio::time::sleep(Duration::from_secs(1)).await;

        let ingress_addr = handle
            .workload
            .ingress_address()
            .ok_or_else(|| anyhow!("ingress listen address unavailable"))?;
        let client = Client::new();
        let url = format!("http://{ingress_addr}/hello");
        let body = wait_for_ingress_response(&client, &url, &ingress_host, Duration::from_secs(20))
            .await?;
        assert_eq!(body, app_body);

        let body = wait_for_ingress_response(&client, &url, &service_host, Duration::from_secs(20))
            .await?;
        assert_eq!(body, app_body);
        Ok(())
    }
    .await;

    if let Some(tx) = sidecar_shutdown.take() {
        let _ = tx.send(());
    }
    if let Some(task) = sidecar_task.take() {
        let _ = task.await;
    }
    if let Some(server) = app_server.take() {
        server.abort();
        let _ = server.await;
    }
    handle.workload.close().await;
    test_result
}

/// Tests that a sidecar with egress enabled discovers the proxy provider via DHT.
///
/// This test verifies:
/// 1. Proxy node announces itself as provider for `podmesh-proxy-node` key
/// 2. Sidecar with enable_egress=true queries DHT for proxy providers
/// 3. Sidecar receives ProxyPeerDiscovered event with proxy's peer ID
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn sidecar_discovers_egress_proxy_via_dht() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();
    
    // Start a workload node that acts as the proxy provider
    let mut handle = start_workload(Vec::new(), true, false)?;
    let proxy_peer_id = handle.peer_id.clone();
    
    let test_result: Result<()> = async {
        // Wait for proxy to be ready and announce itself
        wait_for_kad_ready(handle.kad_rx(), Duration::from_secs(10)).await?;
        wait_for_proxy_provider(handle.proxy_provider_rx(), Duration::from_secs(10)).await?;
        
        // Create sidecar with egress enabled
        let (mut sidecar_cfg, _, _) = build_sidecar_config_with_egress(
            vec![handle.bootstrap_addr.clone()],
            DEFAULT_SIDECAR_APP_PORT,
            true, // enable_egress
        )?;
        // Proxy discovery is all this test needs; skip nftables so the test works without CAP_NET_ADMIN
        sidecar_cfg.skip_egress_nft = true;
        // Use a faster lookup interval for testing
        sidecar_cfg.lookup_interval = Duration::from_secs(1);
        
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        
        let sidecar_task = tokio::spawn(async move {
            run_sidecar_with_shutdown(sidecar_cfg, shutdown_rx, Some(event_tx))
                .await
                .expect("sidecar run");
        });
        
        // Wait for sidecar to discover the proxy peer
        let mut connected = false;
        let mut proxy_discovered = false;
        let deadline = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(deadline);
        
        while !(connected && proxy_discovered) {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    match event {
                        SidecarEvent::Connected { peer_id } => {
                            if peer_id == proxy_peer_id {
                                connected = true;
                                log::info!("sidecar connected to proxy peer={}", peer_id);
                            }
                        }
                        SidecarEvent::ProxyPeerDiscovered { peer_id } => {
                            if peer_id == proxy_peer_id {
                                proxy_discovered = true;
                                log::info!("sidecar discovered proxy peer for egress peer={}", peer_id);
                            }
                        }
                        _ => {}
                    }
                }
                _ = &mut deadline => {
                    break;
                }
            }
        }
        
        assert!(connected, "sidecar never connected to proxy node");
        assert!(proxy_discovered, "sidecar never discovered proxy peer {} for egress", proxy_peer_id);
        
        let _ = shutdown_tx.send(());
        let _ = sidecar_task.await;
        
        Ok(())
    }
    .await;
    
    handle.workload.close().await;
    test_result
}

/// Tests that HTTP traffic can be routed through the sidecar's HTTP CONNECT proxy
/// to an external HTTP server via the egress tunnel through the proxy node.
///
/// This test verifies the complete egress path:
/// 1. Proxy node announces itself and handles egress tunnel streams
/// 2. Sidecar discovers proxy and starts HTTP CONNECT proxy
/// 3. HTTP client uses sidecar's HTTP CONNECT proxy
/// 4. Traffic flows: client -> sidecar HTTP proxy -> P2P tunnel -> proxy node -> target HTTP server
/// 5. Response flows back through the same path
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn egress_http_proxy_routes_traffic_through_tunnel() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();
    
    // Start target HTTP server
    let target_port = allocate_tcp_port();
    let target_body = "hello-from-egress-target".to_string();
    let target_server = spawn_test_app(target_port, target_body.clone()).await?;
    
    // Start proxy node that will handle egress tunnel streams
    let mut handle = start_workload(Vec::new(), true, false)?;
    let proxy_peer_id = handle.peer_id.clone();
    
    let test_result: Result<()> = async {
        // Wait for proxy to be ready
        wait_for_kad_ready(handle.kad_rx(), Duration::from_secs(10)).await?;
        wait_for_proxy_provider(handle.proxy_provider_rx(), Duration::from_secs(10)).await?;
        
        // Allocate port for sidecar's HTTP CONNECT proxy
        let http_proxy_port = allocate_tcp_port();
        
        // Create sidecar with HTTP proxy enabled (no transparent proxy/NFT rules needed)
        let (mut sidecar_cfg, _, _) = build_sidecar_config_full(
            vec![handle.bootstrap_addr.clone()],
            DEFAULT_SIDECAR_APP_PORT,
            false, // enable_egress=false: transparent proxy not needed, http_proxy_port triggers DHT lookups
            Some(http_proxy_port),
        )?;
        sidecar_cfg.lookup_interval = Duration::from_secs(1);
        
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        
        let sidecar_task = tokio::spawn(async move {
            run_sidecar_with_shutdown(sidecar_cfg, shutdown_rx, Some(event_tx))
                .await
                .expect("sidecar run");
        });
        
        // Wait for sidecar to discover the proxy peer
        let mut proxy_discovered = false;
        let deadline = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(deadline);
        
        while !proxy_discovered {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    match event {
                        SidecarEvent::ProxyPeerDiscovered { peer_id } => {
                            if peer_id == proxy_peer_id {
                                proxy_discovered = true;
                                log::info!("sidecar discovered proxy peer for egress peer={}", peer_id);
                            }
                        }
                        _ => {}
                    }
                }
                _ = &mut deadline => {
                    break;
                }
            }
        }
        
        assert!(proxy_discovered, "sidecar never discovered proxy peer {} for egress", proxy_peer_id);
        
        // Give the HTTP CONNECT proxy a moment to start
        tokio::time::sleep(Duration::from_millis(500)).await;
        
        // Create HTTP client that uses the sidecar's HTTP CONNECT proxy
        let proxy_url = format!("http://127.0.0.1:{}", http_proxy_port);
        let client = Client::builder()
            .proxy(reqwest::Proxy::all(&proxy_url).expect("valid proxy URL"))
            .build()
            .expect("build http client with proxy");
        
        // Make HTTP request through the proxy tunnel
        let target_url = format!("http://127.0.0.1:{}/hello", target_port);
        let response = tokio::time::timeout(
            Duration::from_secs(15),
            client.get(&target_url).send()
        )
        .await
        .map_err(|_| anyhow!("HTTP request through egress proxy timed out"))?
        .map_err(|e| anyhow!("HTTP request through egress proxy failed: {}", e))?;
        
        assert!(response.status().is_success(), "expected success status, got {}", response.status());
        
        let body = response.text().await?;
        assert_eq!(body, target_body, "response body mismatch");
        
        log::info!("egress HTTP proxy test succeeded - traffic routed through tunnel");
        
        let _ = shutdown_tx.send(());
        let _ = sidecar_task.await;
        
        Ok(())
    }
    .await;
    
    target_server.abort();
    let _ = target_server.await;
    handle.workload.close().await;
    test_result
}

struct WorkloadHandle {
    workload: Workload,
    peer_id: String,
    bootstrap_addr: String,
    kad_rx: watch::Receiver<bool>,
    proxy_provider_rx: watch::Receiver<bool>,
}

impl WorkloadHandle {
    fn kad_rx(&self) -> watch::Receiver<bool> {
        self.kad_rx.clone()
    }

    fn proxy_provider_rx(&self) -> watch::Receiver<bool> {
        self.proxy_provider_rx.clone()
    }
}

fn start_workload(
    bootstrap_peers: Vec<String>,
    enable_proxy_provider: bool,
    enable_ingress: bool,
) -> Result<WorkloadHandle> {
    let libp2p_port = allocate_udp_port();
    let rest_port = allocate_tcp_port();
    let config = build_workload_config(
        libp2p_port,
        rest_port,
        bootstrap_peers,
        enable_proxy_provider,
        enable_ingress,
    );
    let mut workload = Workload::new(config)?;
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

    Ok(WorkloadHandle {
        workload,
        peer_id,
        bootstrap_addr,
        kad_rx,
        proxy_provider_rx,
    })
}

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
    Ok(spawn_tcp_listener(listener, router, "workplane-test-app"))
}

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
                Some(SidecarEvent::ProxyPeerDiscovered { .. })
                | Some(SidecarEvent::EgressTunnelEstablished { .. })
                | Some(SidecarEvent::EgressTunnelFailed { .. }) => {
                    // Ignore these events in this helper
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

fn build_sidecar_config(
    bootstrap_peers: Vec<String>,
    app_port: u16,
) -> Result<(SidecarConfig, String, String)> {
    build_sidecar_config_with_egress(bootstrap_peers, app_port, false)
}

fn build_sidecar_config_with_egress(
    bootstrap_peers: Vec<String>,
    app_port: u16,
    enable_egress: bool,
) -> Result<(SidecarConfig, String, String)> {
    build_sidecar_config_full(bootstrap_peers, app_port, enable_egress, None)
}

fn build_sidecar_config_full(
    bootstrap_peers: Vec<String>,
    app_port: u16,
    enable_egress: bool,
    http_proxy_port: Option<u16>,
) -> Result<(SidecarConfig, String, String)> {
    let (routes, ingress_host, service_host) = demo_routes(app_port)?;
    let cfg = SidecarConfig {
        provider_label: SIDECAR_PROVIDER_LABEL.to_string(),
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
        enable_egress,
        skip_egress_nft: false,
        http_proxy_port,
    };
    Ok((cfg, ingress_host, service_host))
}

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
