use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tracing::error;
use tracing_subscriber::EnvFilter;

use workplane_gateway::{GatewayConfig, run_gateway, split_csv};

#[derive(Parser, Debug)]
#[command(
    name = "workplane-gateway",
    author,
    version,
    about = "Beemesh Workplane gateway sidecar"
)]
struct Args {
    #[arg(long, env = "namespace")]
    namespace: Option<String>,
    #[arg(long = "workload", env = "workload_name")]
    workload_name: Option<String>,
    #[arg(long, env = "provider_key")]
    provider_key: Option<String>,
    #[arg(long, env = "bootstrap_peers")]
    bootstrap_peers: Option<String>,
    #[arg(long = "bootstrap_ip", env = "bootstrap_ip")]
    bootstrap_peer_ip: Option<String>,
    #[arg(long, env = "lookup_interval_secs", default_value_t = 15)]
    lookup_interval_secs: u64,
    #[arg(
        long = "announce-interval-secs",
        env = "announce_interval_secs",
        default_value_t = 60
    )]
    announce_interval_secs: u64,
    #[arg(long = "libp2p-host", env = "libp2p_host", default_value = "0.0.0.0")]
    libp2p_host: String,
    #[arg(long = "libp2p-port", env = "libp2p_port", default_value_t = 0)]
    libp2p_port: u16,
}

impl TryFrom<Args> for GatewayConfig {
    type Error = anyhow::Error;

    fn try_from(args: Args) -> Result<Self> {
        let label = if let Some(key) = args.provider_key.filter(|s| !s.is_empty()) {
            key
        } else {
            let ns = args
                .namespace
                .filter(|s| !s.is_empty())
                .unwrap_or_default();
            let workload = args
                .workload_name
                .filter(|s| !s.is_empty())
                .unwrap_or_default();
            format!("{ns}/{workload}")
        };

        Ok(Self {
            provider_label: label,
            bootstrap_peers: split_csv(args.bootstrap_peers),
            bootstrap_peer_ip: args.bootstrap_peer_ip.filter(|s| !s.is_empty()),
            lookup_interval: Duration::from_secs(args.lookup_interval_secs.max(1)),
            announce_interval: Duration::from_secs(args.announce_interval_secs.max(1)),
            libp2p_host: args.libp2p_host,
            libp2p_port: args.libp2p_port,
        })
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        error!(error = %err, "workplane gateway failed");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let cfg = GatewayConfig::try_from(args)?;
    run_gateway(cfg).await
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .try_init();
}
