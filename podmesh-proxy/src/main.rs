use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

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
        error!(error = %err, "workplane failed");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    init_tracing();

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
        info!(%peer_id, "workplane bootstrap node started");
    } else {
        info!("workplane bootstrap node started");
    }

    signal::ctrl_c().await.context("wait for ctrl+c")?;

    info!("shutting down workplane bootstrap node");
    workload.close().await;
    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .try_init();
}
