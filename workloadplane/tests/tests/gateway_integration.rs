use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, oneshot};
use workplane::{Config, Workload};
use workplane_gateway::{GatewayConfig, GatewayEvent, run_gateway_with_shutdown};

use workplane_integration::support::{allocate_tcp_port, allocate_udp_port, init_tracing};

fn build_workload_config(libp2p_port: u16, rest_port: u16) -> Config {
    Config {
        bootstrap_peer_strings: Vec::new(),
        libp2p_quic_port: libp2p_port,
        libp2p_host: "127.0.0.1".to_string(),
        rest_host: "127.0.0.1".to_string(),
        rest_port,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_discovers_workload_provider() -> Result<()> {
    init_tracing();
    let libp2p_port = allocate_udp_port();
    let rest_port = allocate_tcp_port();

    let config = build_workload_config(libp2p_port, rest_port);
    let mut workload = Workload::new(config)?;
    workload.start()?;

    let workload_peer_id = workload
        .peer_id()
        .expect("workload peer id available")
        .to_string();
    println!("workload peer id: {workload_peer_id}");
    let bootstrap_addr = format!("/ip4/127.0.0.1/udp/{libp2p_port}/quic-v1");

    let gateway_cfg = GatewayConfig {
        provider_label: "test-namespace/demo-workload".to_string(),
        bootstrap_peers: vec![bootstrap_addr],
        bootstrap_peer_ip: None,
        lookup_interval: Duration::from_secs(2),
        announce_interval: Duration::from_secs(5),
        libp2p_host: "0.0.0.0".to_string(),
        libp2p_port: 0,
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();

    let gateway_task = tokio::spawn(async move {
        run_gateway_with_shutdown(gateway_cfg, shutdown_rx, Some(event_tx))
            .await
            .expect("gateway run");
    });

    let mut connected = false;
    let deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(deadline);

    while !connected {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                match event {
                    GatewayEvent::Connected { ref peer_id } => {
                        println!("gateway connected to {peer_id}");
                        if peer_id == &workload_peer_id {
                            connected = true;
                        }
                    }
                    GatewayEvent::ProviderDiscovered { .. } => {}
                }
            }
            _ = &mut deadline => {
                break;
            }
        }
    }

    assert!(
        connected,
        "gateway never connected to the workload bootstrap node"
    );

    let _ = shutdown_tx.send(());
    let _ = gateway_task.await;

    workload.close().await;
    Ok(())
}
