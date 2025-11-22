use std::net::{TcpListener, UdpSocket};
use std::sync::Once;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::Deserialize;
use tokio::sync::watch::Receiver;
use tokio::time::{Instant, sleep};
use meshproxy::{Config, Workload};

static INIT_TRACING: Once = Once::new();

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workload_mesh_bootstraps_three_nodes() -> Result<()> {
    init_tracing();

    let mut nodes = Vec::new();
    let test_result: Result<()> = async {
        let node1 = start_node(
            allocate_udp_port(),
            allocate_tcp_port(),
            Vec::new(),
            false,
            false,
        )
        .await?;
        let bootstrap1 = node1.bootstrap_multiaddr();
        nodes.push(node1);

        let node2 = start_node(
            allocate_udp_port(),
            allocate_tcp_port(),
            vec![bootstrap1.clone()],
            false,
            false,
        )
        .await?;
        let bootstrap2 = node2.bootstrap_multiaddr();
        nodes.push(node2);

        let node3 = start_node(
            allocate_udp_port(),
            allocate_tcp_port(),
            vec![bootstrap1, bootstrap2],
            true,
            true,
        )
        .await?;
        nodes.push(node3);

        wait_for_mesh(&nodes, 2, Duration::from_secs(30)).await?;
        wait_for_kademlia(&nodes, Duration::from_secs(30)).await?;
        wait_for_proxy_provider(&nodes[2], Duration::from_secs(30)).await?;
        Ok(())
    }
    .await;

    for node in nodes.iter_mut() {
        node.workload.close().await;
    }

    test_result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workload_mesh_single_node_reports_zero_peers() -> Result<()> {
    init_tracing();

    let mut node = start_node(
        allocate_udp_port(),
        allocate_tcp_port(),
        Vec::new(),
        false,
        false,
    )
    .await?;

    let client = reqwest::Client::new();
    wait_for_peer_count(&node, 0, Duration::from_secs(10), &client).await?;
    let observed = fetch_peer_count(&node, &client).await?;
    assert_eq!(observed, 0, "single node workload should see no mesh peers");

    wait_for_kad_ready(node.kad_rx(), Duration::from_secs(5)).await?;

    node.workload.close().await;
    Ok(())
}

async fn start_node(
    libp2p_port: u16,
    rest_port: u16,
    bootstrap_peers: Vec<String>,
    enable_proxy_provider: bool,
    enable_ingress: bool,
) -> Result<NodeHandle> {
    let cfg = Config {
        bootstrap_peer_strings: bootstrap_peers,
        libp2p_quic_port: libp2p_port,
        libp2p_host: "127.0.0.1".to_string(),
        rest_host: "127.0.0.1".to_string(),
        rest_port,
        disable_rest_api: false,
        enable_proxy_provider,
        enable_ingress,
    };

    let mut workload = Workload::new(cfg)?;
    workload.start()?;
    let peer_id = workload
        .peer_id()
        .map(|p| p.to_string())
        .ok_or_else(|| anyhow!("workload peer id unavailable"))?;
    let kad_rx = workload
        .kad_bootstrap_rx()
        .ok_or_else(|| anyhow!("kad bootstrap channel missing"))?;
    let proxy_provider_rx = workload
        .proxy_provider_announced_rx()
        .ok_or_else(|| anyhow!("proxy provider channel missing"))?;

    Ok(NodeHandle {
        workload,
        rest_port,
        libp2p_port,
        peer_id,
        kad_rx,
        proxy_provider_rx,
    })
}

struct NodeHandle {
    workload: Workload,
    rest_port: u16,
    libp2p_port: u16,
    peer_id: String,
    kad_rx: Receiver<bool>,
    proxy_provider_rx: Receiver<bool>,
}

impl NodeHandle {
    fn bootstrap_multiaddr(&self) -> String {
        format!(
            "/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{}",
            self.libp2p_port, self.peer_id
        )
    }

    fn health_url(&self) -> String {
        format!("http://127.0.0.1:{}/healthz", self.rest_port)
    }

    fn kad_rx(&self) -> Receiver<bool> {
        self.kad_rx.clone()
    }

    fn proxy_provider_rx(&self) -> Receiver<bool> {
        self.proxy_provider_rx.clone()
    }
}

#[derive(Deserialize)]
struct HealthResponse {
    peer_count: usize,
}

async fn wait_for_mesh(
    nodes: &[NodeHandle],
    expected_peers: usize,
    timeout: Duration,
) -> Result<()> {
    let client = reqwest::Client::new();
    for node in nodes {
        wait_for_peer_count(node, expected_peers, timeout, &client).await?;
    }
    Ok(())
}

async fn wait_for_peer_count(
    node: &NodeHandle,
    expected: usize,
    timeout: Duration,
    client: &reqwest::Client,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<anyhow::Error> = None;

    loop {
        if Instant::now() > deadline {
            return Err(anyhow!(
                "peer count for {} never reached {expected}. last error: {:?}",
                node.health_url(),
                last_err
            ));
        }

        match fetch_peer_count(node, client).await {
            Ok(count) if count >= expected => return Ok(()),
            Ok(_) => {}
            Err(err) => last_err = Some(err),
        }

        sleep(Duration::from_millis(250)).await;
    }
}

async fn fetch_peer_count(node: &NodeHandle, client: &reqwest::Client) -> Result<usize> {
    let resp = client
        .get(node.health_url())
        .send()
        .await?
        .json::<HealthResponse>()
        .await?;
    Ok(resp.peer_count)
}

async fn wait_for_kademlia(nodes: &[NodeHandle], timeout: Duration) -> Result<()> {
    for node in nodes {
        wait_for_kad_ready(node.kad_rx(), timeout).await?;
    }
    Ok(())
}

async fn wait_for_kad_ready(mut rx: Receiver<bool>, timeout: Duration) -> Result<()> {
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

async fn wait_for_proxy_provider(node: &NodeHandle, timeout: Duration) -> Result<()> {
    let mut rx = node.proxy_provider_rx();
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

fn init_tracing() {
    INIT_TRACING.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn,workplane=info".into()),
            )
            .with_target(false)
            .try_init();
    });
}

fn allocate_udp_port() -> u16 {
    UdpSocket::bind(("127.0.0.1", 0))
        .expect("bind udp port")
        .local_addr()
        .expect("udp local addr")
        .port()
}

fn allocate_tcp_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind tcp port")
        .local_addr()
        .expect("tcp local addr")
        .port()
}
