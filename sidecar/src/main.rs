use std::{fs, io::ErrorKind, path::Path, time::Duration};

use anyhow::{Context, Result};
use base64::Engine;
use clap::Parser;
use tracing::error;
use tracing_subscriber::EnvFilter;

use protocol::gateway_metadata::{DEFAULT_GATEWAY_BOOTSTRAP_MULTIADDR, GatewaySidecarMetadata};
use sidecar::{
    DEFAULT_GATEWAY_APP_PORT, GatewayConfig, manifest_routes::extract_gateway_routes, run_gateway,
    split_csv,
};

#[derive(Parser, Debug)]
#[command(
    name = "sidecar",
    author,
    version,
    about = " Podmesh sidecar"
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
        env = "PODMESH_GATEWAY_METADATA_PATH",
        default_value = "/var/run/podmesh/gateway/metadata.json"
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

        let metadata = load_metadata(&args.metadata_path)?
            .ok_or_else(|| anyhow::anyhow!("gateway metadata missing at {}", args.metadata_path))?;

        let manifest_bytes = base64::engine::general_purpose::STANDARD
            .decode(&metadata.manifest_b64)
            .context("failed to decode manifest payload from metadata")?;
        let manifest_id = metadata.manifest_id.clone();
        let ingress_host = format!("{}.mesh.local", manifest_id);

        let extraction = extract_gateway_routes(&manifest_bytes, &manifest_id)
            .with_context(|| format!("failed to extract routes for manifest {}", manifest_id))?;

        let mut bootstrap_peers = vec![metadata.bootstrap_peer.clone()];
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
            routes: extraction.routes,
            owner_public_key_b64: metadata.owner_public_key_b64.clone(),
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
