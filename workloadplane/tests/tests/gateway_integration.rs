use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, oneshot, watch};
use workplane::{Config, Workload};
use workplane_gateway::{GatewayConfig, GatewayEvent, run_gateway_with_shutdown};

use workplane_integration::support::{allocate_tcp_port, allocate_udp_port, init_tracing};

const PROXY_PROVIDER_LABEL: &str = "beemesh-proxy-node";

fn build_workload_config(
    libp2p_port: u16,
    rest_port: u16,
    bootstrap_peer_strings: Vec<String>,
    enable_proxy_provider: bool,
) -> Config {
    Config {
        bootstrap_peer_strings,
        libp2p_quic_port: libp2p_port,
        libp2p_host: "127.0.0.1".to_string(),
        rest_host: "127.0.0.1".to_string(),
        rest_port,
        disable_rest_api: false,
        enable_proxy_provider,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_discovers_workload_provider() -> Result<()> {
    init_tracing();
    let mut workloads = Vec::new();

    let test_result: Result<()> = async {
        let first = start_workload(Vec::new(), true)?;
        let provider_peer_id = first.peer_id.clone();
        let first_bootstrap = first.bootstrap_addr.clone();
        workloads.push(first);

        let second = start_workload(vec![first_bootstrap.clone()], false)?;
        let second_bootstrap = second.bootstrap_addr.clone();
        workloads.push(second);

        let third = start_workload(vec![first_bootstrap, second_bootstrap], false)?;
        workloads.push(third);

        for node in &workloads {
            wait_for_kad_ready(node.kad_rx(), Duration::from_secs(20)).await?;
        }

        if let Some(provider_node) = workloads.first() {
            wait_for_proxy_provider(provider_node.proxy_provider_rx(), Duration::from_secs(10))
                .await?;
        }

        let gateway_bootstrap_peers: Vec<String> =
            workloads.iter().map(|node| node.bootstrap_addr.clone()).collect();

        tokio::time::sleep(Duration::from_secs(1)).await;

        let gateway_cfg = GatewayConfig {
            provider_label: PROXY_PROVIDER_LABEL.to_string(),
            bootstrap_peers: gateway_bootstrap_peers,
            bootstrap_peer_ip: None,
            lookup_interval: Duration::from_secs(2),
            announce_interval: Duration::from_secs(5),
            libp2p_host: "0.0.0.0".to_string(),
            libp2p_port: 0,
            announce_providers: false,
        };

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let gateway_task = tokio::spawn(async move {
            run_gateway_with_shutdown(gateway_cfg, shutdown_rx, Some(event_tx))
                .await
                .expect("gateway run");
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
                        GatewayEvent::Connected { ref peer_id } => {
                            if known_peer_ids.contains(peer_id) {
                                connected_peers.insert(peer_id.clone());
                            }
                        }
                        GatewayEvent::ProviderDiscovered { ref peer_id } => {
                            if peer_id == &provider_peer_id {
                                provider_seen = true;
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
            "gateway never connected to any workload peer"
        );
        assert!(
            provider_seen,
            "gateway never observed provider {}",
            provider_peer_id
        );

        let _ = shutdown_tx.send(());
        let _ = gateway_task.await;

        Ok(())
    }
    .await;

    for node in workloads.iter_mut() {
        node.workload.close().await;
    }

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
) -> Result<WorkloadHandle> {
    let libp2p_port = allocate_udp_port();
    let rest_port = allocate_tcp_port();
    let config = build_workload_config(libp2p_port, rest_port, bootstrap_peers, enable_proxy_provider);
    let mut workload = Workload::new(config)?;
    workload.start()?;

    let peer_id = workload
        .peer_id()
        .map(|id| id.to_string())
        .ok_or_else(|| anyhow!("workload peer id unavailable"))?;
    let bootstrap_addr = format!(
        "/ip4/127.0.0.1/udp/{libp2p_port}/quic-v1/p2p/{peer_id}"
    );
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
