use anyhow::Result;
use clap::Parser;
use tokio::signal;
use log::{error, info};

use podmesh_proxy::{Config, Workload};

#[derive(Parser, Debug)]
#[command(name = "podmesh-proxy", author, version, about = "Podmesh proxy")]
struct Args {
    #[arg(long = "bootstrap-peer", env = "bootstrap_peer", value_delimiter = ',')]
    bootstrap_peer: Vec<String>,
    #[arg(long = "libp2p-host", env = "libp2p_host", default_value = "0.0.0.0")]
    libp2p_host: String,
    #[arg(
        long = "libp2p-quic-port",
        env = "libp2p_quic_port",
        default_value_t = 0
    )]
    libp2p_quic_port: u16,
    #[arg(
        long = "rest-api-host",
        env = "rest_api_host",
        default_value = "0.0.0.0"
    )]
    rest_host: String,
    #[arg(long = "rest-api-port", env = "rest_api_port", default_value_t = 7100)]
    rest_port: u16,
    #[arg(long = "disable-rest-api", default_value_t = false)]
    disable_rest_api: bool,
    #[arg(
        long = "enable-proxy-provider",
        env = "enable_proxy_provider",
        default_value_t = false
    )]
    enable_proxy_provider: bool,
    #[arg(
        long = "enable-ingress",
        env = "enable_ingress",
        default_value_t = false
    )]
    enable_ingress: bool,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        error!("workplane failed: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    env_logger::init();

    let Args {
        bootstrap_peer,
        libp2p_host,
        libp2p_quic_port,
        rest_host,
        rest_port,
        disable_rest_api,
        enable_proxy_provider,
        enable_ingress,
    } = Args::parse();

    let mut cfg = Config {
        bootstrap_peer_strings: bootstrap_peer,
        libp2p_host,
        libp2p_quic_port,
        rest_host,
        rest_port,
        disable_rest_api,
        enable_proxy_provider,
        enable_ingress,
    };
    cfg.apply_defaults();

    let mut workload = Workload::new(cfg)?;
    workload.start()?;

    if let Some(peer_id) = workload.peer_id() {
        info!("workplane bootstrap node started peer_id={}", peer_id);
    } else {
        info!("workplane bootstrap node started");
    }

    // Wait for either SIGTERM or SIGINT to gracefully shutdown
    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("received SIGINT");
        }
        _ = async {
            #[cfg(unix)]
            {
                let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler");
                sigterm.recv().await;
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await;
            }
        } => {
            info!("received SIGTERM");
        }
    }

    info!("shutting down workplane bootstrap node");
    workload.close().await;
    Ok(())
}
