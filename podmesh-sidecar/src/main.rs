use std::{fs, io::ErrorKind, path::Path, time::Duration};

use anyhow::{Context, Result};
use base64::Engine;
use clap::Parser;
use tracing::{error, warn};

use podmesh_sidecar::{
    DEFAULT_GATEWAY_APP_PORT, GatewayConfig, manifest_routes::extract_gateway_routes, run_gateway,
    split_csv,
};
use protocol::{
    gateway_metadata::{DEFAULT_GATEWAY_BOOTSTRAP_MULTIADDR, GatewaySidecarMetadata},
    libp2p_constants::MESH_DOMAIN_SUFFIX,
    machine::{GatewayRouteKind, GatewayRouteSpec},
};

#[derive(Parser, Debug)]
#[command(name = "podmesh-sidecar", author, version, about = " Podmesh sidecar")]
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
    #[arg(long = "metadata-b64", env = "PODMESH_GATEWAY_METADATA_B64")]
    metadata_b64: Option<String>,
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

        let metadata = if let Some(blob) = args
            .metadata_b64
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            decode_inline_metadata(blob)?
        } else {
            load_metadata(&args.metadata_path)?.ok_or_else(|| {
                anyhow::anyhow!("gateway metadata missing at {}", args.metadata_path)
            })?
        };

        let manifest_bytes = base64::engine::general_purpose::STANDARD
            .decode(&metadata.manifest_b64)
            .context("failed to decode manifest payload from metadata")?;

        let extraction = extract_gateway_routes(&manifest_bytes, &metadata.manifest_id)
            .with_context(|| {
                format!(
                    "failed to extract routes for manifest {}",
                    metadata.manifest_id
                )
            })?;

        let mut routes = extraction.routes;
        let (manifest_id, ingress_host, derived_from_ingress) =
            derive_manifest_identity(&routes, &metadata.manifest_id);
        if !derived_from_ingress {
            warn!(
                metadata_manifest = %metadata.manifest_id,
                manifest = %manifest_id,
                "no ingress host detected; using fallback manifest id"
            );
        }
        update_service_route_hosts(&mut routes, &manifest_id);

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
            routes,
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
    tracing_support::init_tracing();
    let args = Args::parse();
    let cfg = GatewayConfig::try_from(args)?;
    run_gateway(cfg).await
}

fn decode_inline_metadata(blob: &str) -> Result<GatewaySidecarMetadata> {
    let trimmed = blob.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("inline gateway metadata blob is empty"));
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .context("failed to decode inline gateway metadata blob")?;

    let metadata: GatewaySidecarMetadata =
        serde_json::from_slice(&decoded).context("failed to parse inline gateway metadata blob")?;
    Ok(metadata)
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

fn derive_manifest_identity(
    routes: &[GatewayRouteSpec],
    metadata_manifest_id: &str,
) -> (String, String, bool) {
    if let Some((manifest_id, host)) = routes
        .iter()
        .filter(|route| matches!(route.source, GatewayRouteKind::Ingress))
        .filter_map(|route| manifest_id_from_host(&route.host).map(|id| (id, route.host.clone())))
        .next()
    {
        return (manifest_id, host, true);
    }

    let manifest_id = sanitize_manifest_id(metadata_manifest_id);
    let ingress_host = format!("{}.{}", manifest_id, MESH_DOMAIN_SUFFIX);
    (manifest_id, ingress_host, false)
}

fn manifest_id_from_host(host: &str) -> Option<String> {
    let suffix = format!(".{}", MESH_DOMAIN_SUFFIX);
    let normalized = host.trim().trim_end_matches('.').to_lowercase();
    if let Some(without_suffix) = normalized.strip_suffix(&suffix) {
        let trimmed = without_suffix.trim_matches('.');
        return trimmed
            .rsplit('.')
            .next()
            .map(String::from)
            .filter(|segment| !segment.is_empty());
    }
    (!normalized.is_empty()).then_some(normalized)
}

fn sanitize_manifest_id(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;

    for ch in value.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '-' {
            slug.push(lower);
            last_dash = lower == '-';
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }

    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "workload".into()
    } else {
        trimmed.to_string()
    }
}

fn update_service_route_hosts(routes: &mut [GatewayRouteSpec], manifest_id: &str) {
    for route in routes.iter_mut() {
        if matches!(route.source, GatewayRouteKind::Service) {
            route.host = format!(
                "{}.{}.{}",
                route.service_name, manifest_id, MESH_DOMAIN_SUFFIX
            )
            .to_lowercase();
        }
    }
}
