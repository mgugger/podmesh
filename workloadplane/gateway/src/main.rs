use std::{fs, io::ErrorKind, path::Path, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use tracing::error;
use tracing_subscriber::EnvFilter;

use protocol::{
    gateway_metadata::{DEFAULT_GATEWAY_BOOTSTRAP_MULTIADDR, GatewaySidecarMetadata},
    libp2p_constants::DEFAULT_INGRESS_MANIFEST_ID,
    machine::GatewayRouteSpec,
};
use workplane_gateway::{DEFAULT_GATEWAY_APP_PORT, GatewayConfig, run_gateway, split_csv};

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
    #[arg(
        long,
        env = "bootstrap_peers",
        default_value = DEFAULT_GATEWAY_BOOTSTRAP_MULTIADDR
    )]
    bootstrap_peers: String,
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
    #[arg(
        long = "metadata-path",
        env = "BEEMESH_GATEWAY_METADATA_PATH",
        default_value = "/var/run/beemesh/gateway/metadata.json"
    )]
    metadata_path: String,
}

impl TryFrom<Args> for GatewayConfig {
    type Error = anyhow::Error;

    fn try_from(args: Args) -> Result<Self> {
        let label = if let Some(key) = args.provider_key.filter(|s| !s.is_empty()) {
            key
        } else {
            let ns = args.namespace.filter(|s| !s.is_empty()).unwrap_or_default();
            let workload = args
                .workload_name
                .filter(|s| !s.is_empty())
                .unwrap_or_default();
            format!("{ns}/{workload}")
        };

        let metadata = load_metadata(&args.metadata_path)?;

        let manifest_id = metadata
            .as_ref()
            .map(|m| m.manifest_id.clone())
            .unwrap_or_else(|| DEFAULT_INGRESS_MANIFEST_ID.to_string());
        let ingress_host = format!("{}.mesh.local", manifest_id);

        let mut bootstrap_peers = metadata
            .as_ref()
            .map(|m| vec![m.bootstrap_peer.clone()])
            .unwrap_or_default();
        bootstrap_peers.extend(split_csv(Some(args.bootstrap_peers)));

        Ok(Self {
            provider_label: label,
            bootstrap_peers,
            bootstrap_peer_ip: args.bootstrap_peer_ip.filter(|s| !s.is_empty()),
            lookup_interval: Duration::from_secs(args.lookup_interval_secs.max(1)),
            announce_interval: Duration::from_secs(args.announce_interval_secs.max(1)),
            libp2p_host: args.libp2p_host,
            libp2p_port: args.libp2p_port,
            announce_providers: true,
            manifest_id,
            ingress_host,
            app_port: DEFAULT_GATEWAY_APP_PORT,
            routes: vec![GatewayRouteSpec {
                path_prefix: "/".to_string(),
                target_port: DEFAULT_GATEWAY_APP_PORT,
            }],
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

fn load_metadata(path: &str) -> Result<Option<GatewaySidecarMetadata>> {
    let metadata_path = Path::new(path);
    match fs::read(metadata_path) {
        Ok(bytes) => {
            if bytes.is_empty() {
                return Ok(None);
            }
            let metadata: GatewaySidecarMetadata = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse gateway metadata at {}", path))?;
            Ok(Some(metadata))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(anyhow::anyhow!(
            "failed to read gateway metadata file {}: {}",
            metadata_path.display(),
            err
        )),
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .try_init();
}
