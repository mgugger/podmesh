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

use podmesh_integration_tests::support::{
    allocate_tcp_port, allocate_udp_port, fresh_tenant_owner, init_ephemeral_keys, init_tracing,
    provision_proxy_cert,
};

const DEMO_MANIFEST_ID: &str = "demo-nginx";
const DEMO_MANIFEST: &[u8] = include_bytes!("../sample_manifests/demo_deployment.yml");

fn build_workload_config(
    libp2p_port: u16,
    rest_port: u16,
    proxy_peer_multiaddrs: Vec<String>,
    enable_ingress: bool,
) -> Config {
    Config {
        proxy_peer_multiaddrs,
        identity: podmesh_proxy::IdentitySource::ephemeral(),
        libp2p_quic_port: libp2p_port,
        libp2p_host: "127.0.0.1".to_string(),
        rest_host: "127.0.0.1".to_string(),
        rest_port,
        disable_rest_api: false,
        enable_ingress,
        owner_pubkey: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ingress_proxies_requests_via_sidecar() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();
    let (owner_b64, owner_sk, owner_pk) = fresh_tenant_owner();
    let mut handle = start_workload(Vec::new(), true)?;
    let mut sidecar_shutdown: Option<oneshot::Sender<()>> = None;
    let mut sidecar_task: Option<JoinHandle<()>> = None;
    let mut app_server: Option<JoinHandle<()>> = None;
    let test_result: Result<()> = async {
        wait_for_network_ready(handle.network_ready_rx(), Duration::from_secs(10)).await?;
        wait_for_network_ready(handle.network_ready_rx(), Duration::from_secs(10)).await?;
        provision_proxy_cert(
            handle.rest_port,
            &owner_pk,
            &owner_sk,
            Duration::from_secs(10),
        )
        .await?;

        let app_port = allocate_tcp_port();

        let app_body = "hello-from-proxied-app".to_string();
        app_server = Some(spawn_test_app(app_port, app_body.clone()).await?);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        sidecar_shutdown = Some(shutdown_tx);
        let (mut sidecar_cfg, ingress_host, service_host) =
            build_sidecar_config(vec![handle.bootstrap_addr.clone()], app_port)?;
        sidecar_cfg.owner_public_key_b64 = Some(owner_b64.clone());
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

/// Tests that a sidecar with egress enabled discovers an explicitly configured proxy.
///
/// This test verifies:
/// 1. Proxy node announces itself as provider for `podmesh-proxy-node` key
/// 2. Sidecar connects to the configured stable proxy identity
/// 3. Sidecar receives ProxyPeerDiscovered event with proxy's peer ID
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn sidecar_discovers_explicit_egress_proxy() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();

    // Start a workload node that acts as the proxy provider
    let mut handle = start_workload(Vec::new(), false)?;
    let proxy_peer_id = handle.peer_id.clone();

    let test_result: Result<()> = async {
        // Wait for proxy to be ready and announce itself
        wait_for_network_ready(handle.network_ready_rx(), Duration::from_secs(10)).await?;

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
                        SidecarEvent::ProxyPeerDiscovered { peer_id }
                            if peer_id == proxy_peer_id => {
                            proxy_discovered = true;
                            log::info!("sidecar discovered proxy peer for egress peer={}", peer_id);
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
        assert!(
            proxy_discovered,
            "sidecar never discovered proxy peer {} for egress",
            proxy_peer_id
        );

        let _ = shutdown_tx.send(());
        let _ = sidecar_task.await;

        Ok(())
    }
    .await;

    handle.workload.close().await;
    test_result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn sidecar_fetches_and_registers_with_additional_regional_proxy() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();
    let (owner_b64, owner_sk, owner_pk) = fresh_tenant_owner();

    let mut first = start_workload(Vec::new(), false)?;
    let mut second = start_workload(vec![first.bootstrap_addr.clone()], false)?;
    let second_peer_id = second.peer_id.clone();

    let test_result: Result<()> = async {
        wait_for_network_ready(first.network_ready_rx(), Duration::from_secs(10)).await?;
        wait_for_network_ready(second.network_ready_rx(), Duration::from_secs(10)).await?;
        provision_proxy_cert(
            first.rest_port,
            &owner_pk,
            &owner_sk,
            Duration::from_secs(10),
        )
        .await?;
        provision_proxy_cert(
            second.rest_port,
            &owner_pk,
            &owner_sk,
            Duration::from_secs(10),
        )
        .await?;

        let (mut sidecar_cfg, _, _) =
            build_sidecar_config(vec![first.bootstrap_addr.clone()], DEFAULT_SIDECAR_APP_PORT)?;
        sidecar_cfg.owner_public_key_b64 = Some(owner_b64);
        sidecar_cfg.lookup_interval = Duration::from_secs(1);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let sidecar_task = tokio::spawn(async move {
            run_sidecar_with_shutdown(sidecar_cfg, shutdown_rx, Some(event_tx))
                .await
                .expect("sidecar run");
        });

        let first_table = first.workload.routing_table_handle().unwrap();
        let second_table = second.workload.routing_table_handle().unwrap();
        let registration_deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let first_registered = first_table.read().unwrap().contains_key(DEMO_MANIFEST_ID);
            let second_registered = second_table.read().unwrap().contains_key(DEMO_MANIFEST_ID);
            if first_registered && second_registered {
                break;
            }
            anyhow::ensure!(
                Instant::now() < registration_deadline,
                "sidecar did not discover {} and register routes with both regional proxies",
                second_peer_id
            );
            tokio::task::yield_now().await;
        }

        let _ = shutdown_tx.send(());
        let _ = sidecar_task.await;
        Ok(())
    }
    .await;

    first.workload.close().await;
    second.workload.close().await;
    test_result
}

/// Tests that HTTP traffic can be routed through the sidecar's HTTP CONNECT proxy
/// to an external HTTP server via the egress tunnel through the proxy node.
///
/// This test exercises the **tenant-cert-gated** discovery path mandated by the
/// sidecar-proxy-auth spec:
/// 1. Owner keypair is generated for this test
/// 2. Proxy node starts and is provisioned with a tenant-signed `NodeCert` via
///    `podctl::cert::grant_proxy_async` (simulating `podctl grant-proxy`)
/// 3. Sidecar is configured with the same owner pubkey, discovers the proxy via
///    its explicit peer record, verifies the cert
///    during handshake, then routes egress traffic through the verified proxy
/// 4. Response flows back through the tunnel
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn egress_http_proxy_routes_traffic_through_tunnel() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();
    let (owner_b64, owner_sk, owner_pk) = fresh_tenant_owner();

    // Start target HTTP server
    let target_port = allocate_tcp_port();
    let target_body = "hello-from-egress-target".to_string();
    let target_server = spawn_test_app(target_port, target_body.clone()).await?;

    // Start proxy node that will handle egress tunnel streams
    let mut handle = start_workload(Vec::new(), false)?;
    let proxy_peer_id = handle.peer_id.clone();

    let test_result: Result<()> = async {
        // Wait for proxy to be ready
        wait_for_network_ready(handle.network_ready_rx(), Duration::from_secs(10)).await?;

        // Provision the proxy with a tenant-signed NodeCert via the REST API.
        // This is the in-test equivalent of `podctl grant-proxy --proxy-url <url>`.
        provision_proxy_cert(
            handle.rest_port,
            &owner_pk,
            &owner_sk,
            Duration::from_secs(10),
        )
        .await
        .map_err(|e| anyhow!("provision proxy cert failed: {}", e))?;

        // Allocate port for sidecar's HTTP CONNECT proxy
        let http_proxy_port = allocate_tcp_port();

        // Create sidecar with HTTP proxy enabled and tenant owner pubkey set so
        // it authenticates the explicitly configured proxy.
        let (mut sidecar_cfg, _, _) = build_sidecar_config_full(
            vec![handle.bootstrap_addr.clone()],
            DEFAULT_SIDECAR_APP_PORT,
            false,
            Some(http_proxy_port),
        )?;
        sidecar_cfg.owner_public_key_b64 = Some(owner_b64.clone());
        sidecar_cfg.lookup_interval = Duration::from_secs(1);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let sidecar_task = tokio::spawn(async move {
            run_sidecar_with_shutdown(sidecar_cfg, shutdown_rx, Some(event_tx))
                .await
                .expect("sidecar run");
        });

        // Wait for sidecar to discover the tenant-authorized proxy peer.
        let mut proxy_discovered = false;
        let deadline = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(deadline);

        while !proxy_discovered {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    if let SidecarEvent::ProxyPeerDiscovered { peer_id } = event
                        && peer_id == proxy_peer_id
                    {
                        proxy_discovered = true;
                        log::info!("sidecar discovered tenant-bound proxy peer={}", peer_id);
                    }
                }
                _ = &mut deadline => {
                    break;
                }
            }
        }

        assert!(
            proxy_discovered,
            "sidecar never discovered tenant-authorized proxy peer {} for egress",
            proxy_peer_id
        );

        // Give the HTTP CONNECT proxy a moment to start and the sidecar to verify the cert.
        tokio::time::sleep(Duration::from_millis(800)).await;

        // Create HTTP client that uses the sidecar's HTTP CONNECT proxy
        let proxy_url = format!("http://127.0.0.1:{}", http_proxy_port);
        let client = Client::builder()
            .proxy(reqwest::Proxy::all(&proxy_url).expect("valid proxy URL"))
            .build()
            .expect("build http client with proxy");

        // Make HTTP request through the proxy tunnel
        let target_url = format!("http://127.0.0.1:{}/hello", target_port);
        let response =
            tokio::time::timeout(Duration::from_secs(15), client.get(&target_url).send())
                .await
                .map_err(|_| anyhow!("HTTP request through egress proxy timed out"))?
                .map_err(|e| anyhow!("HTTP request through egress proxy failed: {}", e))?;

        assert!(
            response.status().is_success(),
            "expected success status, got {}",
            response.status()
        );

        let body = response.text().await?;
        assert_eq!(body, target_body, "response body mismatch");

        log::info!("egress HTTP proxy test succeeded - traffic routed through tenant-bound tunnel");

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
    network_ready_rx: watch::Receiver<bool>,
    rest_port: u16,
}

impl WorkloadHandle {
    fn network_ready_rx(&self) -> watch::Receiver<bool> {
        self.network_ready_rx.clone()
    }
}

fn start_workload(bootstrap_peers: Vec<String>, enable_ingress: bool) -> Result<WorkloadHandle> {
    let libp2p_port = allocate_udp_port();
    let rest_port = allocate_tcp_port();
    let config = build_workload_config(libp2p_port, rest_port, bootstrap_peers, enable_ingress);
    let mut workload = Workload::new(config)?;
    workload.start()?;

    let peer_id = workload
        .peer_id()
        .map(|id| id.to_string())
        .ok_or_else(|| anyhow!("workload peer id unavailable"))?;
    let bootstrap_addr = format!("/ip4/127.0.0.1/udp/{libp2p_port}/quic-v1/p2p/{peer_id}");
    let network_ready_rx = workload
        .network_ready_rx()
        .ok_or_else(|| anyhow!("network readiness channel missing"))?;

    Ok(WorkloadHandle {
        workload,
        peer_id,
        bootstrap_addr,
        network_ready_rx,
        rest_port,
    })
}

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
            "sidecar did not become ready for proxy {}",
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
        identity: podmesh_sidecar::IdentitySource::ephemeral(),
        proxy_peers: protocol::proxy_peers_from_multiaddrs(&bootstrap_peers)?,
        lookup_interval: Duration::from_secs(2),
        libp2p_host: "0.0.0.0".to_string(),
        libp2p_port: 0,
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

/// End-to-end exercise of the sidecar–proxy auth flow:
///
/// 1. Generate a tenant owner Ed25519 keypair (the operator's key).
/// 2. Start a proxy. Its REST API exposes `signing_pubkey`, `kem_pubkey`,
///    `peer_id` and accepts a `NodeCert` POST.
/// 3. Call `podctl::cert::grant_proxy_async` (the in-process equivalent of
///    `podctl grant-proxy`) to:
///    - Fetch the proxy's keys + peer_id
///    - Build a `NodeRole::Proxy` `NodeCert` signed with the owner key
///    - POST it to the proxy
/// 4. Start a sidecar configured with the same owner pubkey.
/// 5. Assert the sidecar discovers the proxy under the obfuscated tenant
///    explicit proxy record and emits a
///    `ProxyPeerDiscovered` event for it.
/// 6. Assert the proxy's in-memory routing table eventually contains the
///    sidecar registration for the demo manifest, proving the cert-gated
///    handshake + `SidecarRegistration` flow worked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn sidecar_registers_with_tenant_signed_proxy_cert() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();
    let (owner_b64, owner_sk, owner_pk) = fresh_tenant_owner();

    let mut handle = start_workload(Vec::new(), false)?;
    let proxy_peer_id = handle.peer_id.clone();

    let test_result: Result<()> = async {
        wait_for_network_ready(handle.network_ready_rx(), Duration::from_secs(10)).await?;

        // Provision the cert via the REST API. The proxy will:
        //   - verify the owner_sig
        //   - check peer_id matches its own libp2p identity
        //   - check role == Proxy
        //   - store the cert keyed by owner_pubkey
        //   - announce under blake3(owner_pubkey)[..16]
        let ack = provision_proxy_cert(
            handle.rest_port,
            &owner_pk,
            &owner_sk,
            Duration::from_secs(10),
        )
        .await
        .map_err(|e| anyhow!("provision_proxy_cert failed: {}", e))?;
        log::info!(
            "proxy provisioned with cert: owner_pubkey={} valid_until={}",
            ack.owner_pubkey,
            ack.valid_until
        );
        assert!(
            ack.owner_pubkey == owner_b64,
            "expected tenant owner binding in response"
        );

        // Sanity check: the cert should be visible in the proxy's in-process cert store.
        let store = handle
            .workload
            .cert_store()
            .ok_or_else(|| anyhow!("proxy cert_store unavailable"))?;
        {
            let g = store.read().unwrap();
            assert!(
                g.contains_key(&owner_b64),
                "expected proxy cert store to contain owner_pubkey {}",
                owner_b64
            );
        }

        // Build the sidecar with the matching tenant pubkey.
        let app_port = allocate_tcp_port();
        let app_body = "hello-from-tenant-bound-app".to_string();
        let app_server = spawn_test_app(app_port, app_body.clone()).await?;

        let (mut sidecar_cfg, _, _) =
            build_sidecar_config(vec![handle.bootstrap_addr.clone()], app_port)?;
        sidecar_cfg.owner_public_key_b64 = Some(owner_b64.clone());
        sidecar_cfg.lookup_interval = Duration::from_secs(1);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let sidecar_task = tokio::spawn(async move {
            run_sidecar_with_shutdown(sidecar_cfg, shutdown_rx, Some(event_tx))
                .await
                .expect("sidecar run");
        });

        // Wait for the sidecar to discover the proxy under the obfuscated tenant key.
        let mut tenant_proxy_seen = false;
        let proxy_discovery_deadline = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(proxy_discovery_deadline);
        while !tenant_proxy_seen {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    if let SidecarEvent::ProxyPeerDiscovered { peer_id } = event
                        && peer_id == proxy_peer_id
                    {
                        tenant_proxy_seen = true;
                    }
                }
                _ = &mut proxy_discovery_deadline => break,
            }
        }
        assert!(
            tenant_proxy_seen,
            "sidecar never discovered tenant-authorized proxy peer {}",
            proxy_peer_id
        );

        // Wait for the proxy's routing table to contain the sidecar registration.
        // This proves: handshake exchanged → cert verified by sidecar → registration sent →
        // proxy verified registration against stored NodeCert → routes stored.
        let routing_table = handle.workload.routing_table_handle();
        let routing_deadline = Instant::now() + Duration::from_secs(20);
        let mut registered = false;
        while Instant::now() < routing_deadline {
            if let Some(table) = routing_table.as_ref()
                && table.read().unwrap().contains_key(DEMO_MANIFEST_ID)
            {
                registered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            registered,
            "proxy never received a verified SidecarRegistration for manifest {}",
            DEMO_MANIFEST_ID
        );

        let _ = shutdown_tx.send(());
        let _ = sidecar_task.await;
        app_server.abort();
        let _ = app_server.await;
        Ok(())
    }
    .await;

    handle.workload.close().await;
    test_result
}

/// Negative test: when the proxy holds NO tenant cert, the sidecar (with a
/// configured owner pubkey) should NOT be able to send a `SidecarRegistration`
/// because the explicitly configured proxy has no tenant certificate, and even if
/// it dialed by some other means, the proxy would reject the registration.
///
/// This test asserts the proxy's routing table stays empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn sidecar_registration_blocked_when_proxy_has_no_tenant_cert() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();
    let (owner_b64, _owner_sk, _owner_pk) = fresh_tenant_owner();

    let mut handle = start_workload(Vec::new(), false)?;
    let test_result: Result<()> = async {
        wait_for_network_ready(handle.network_ready_rx(), Duration::from_secs(10)).await?;

        // Intentionally do NOT call provision_proxy_cert.

        let app_port = allocate_tcp_port();
        let (mut sidecar_cfg, _, _) =
            build_sidecar_config(vec![handle.bootstrap_addr.clone()], app_port)?;
        sidecar_cfg.owner_public_key_b64 = Some(owner_b64.clone());
        sidecar_cfg.lookup_interval = Duration::from_secs(1);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let sidecar_task = tokio::spawn(async move {
            run_sidecar_with_shutdown(sidecar_cfg, shutdown_rx, Some(event_tx))
                .await
                .expect("sidecar run");
        });

        // Drain events for a few seconds — we don't expect ProxyPeerDiscovered for tenant key.
        let drain_deadline = tokio::time::sleep(Duration::from_secs(8));
        tokio::pin!(drain_deadline);
        loop {
            tokio::select! {
                Some(_event) = event_rx.recv() => {}
                _ = &mut drain_deadline => break,
            }
        }

        let routing_table = handle.workload.routing_table_handle();
        if let Some(table) = routing_table.as_ref() {
            let snap = table.read().unwrap();
            assert!(
                snap.is_empty(),
                "proxy unexpectedly accepted a registration without holding any tenant cert: {:?}",
                snap.keys().collect::<Vec<_>>()
            );
        }

        let _ = shutdown_tx.send(());
        let _ = sidecar_task.await;
        Ok(())
    }
    .await;

    handle.workload.close().await;
    test_result
}
