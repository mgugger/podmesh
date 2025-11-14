use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use workplane::{Config, Workload};

#[derive(Parser, Debug)]
#[command(name = "workplane", author, version, about = "Beemesh Workplane agent")]
struct Args {
    #[arg(long, env = "bootstrap_peers")]
    bootstrap_peers: Option<String>,
    #[arg(long = "libp2p-host", env = "libp2p_host", default_value = "0.0.0.0")]
    libp2p_host: String,
    #[arg(long = "libp2p-port", env = "libp2p_port", default_value_t = 0)]
    libp2p_quic_port: u16,
    #[arg(long = "rest-host", env = "rest_host", default_value = "0.0.0.0")]
    rest_host: String,
    #[arg(long = "rest-port", env = "rest_port", default_value_t = 7100)]
    rest_port: u16,
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

    let args = Args::parse();

    let bootstrap_peers = split_csv(args.bootstrap_peers);

    let mut cfg = Config {
        bootstrap_peer_strings: bootstrap_peers,
        libp2p_host: args.libp2p_host,
        libp2p_quic_port: args.libp2p_quic_port,
        rest_host: args.rest_host,
        rest_port: args.rest_port,
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

fn split_csv(input: Option<String>) -> Vec<String> {
    input
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}
