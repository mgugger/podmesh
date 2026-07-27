use std::{fs, io::ErrorKind, path::Path, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use log::{error, warn};

use podmesh_sidecar::{
    DEFAULT_SIDECAR_APP_PORT, SidecarConfig, manifest_routes::extract_sidecar_routes, run_sidecar,
};
use protocol::{
    libp2p_constants::MESH_DOMAIN_SUFFIX,
    machine::{SidecarRouteKind, SidecarRouteSpec},
    sidecar_metadata::SidecarMetadata,
};

#[derive(Parser, Debug)]
#[command(name = "podmesh-sidecar", author, version, about = " Podmesh sidecar")]
struct Args {
    #[arg(long, env = "lookup_interval_secs", default_value_t = 15)]
    lookup_interval_secs: u64,
    #[arg(long = "libp2p-host", env = "libp2p_host", default_value = "0.0.0.0")]
    libp2p_host: String,
    #[arg(long = "libp2p-port", env = "libp2p_port", default_value_t = 0)]
    libp2p_port: u16,
    #[arg(
        long = "metadata-path",
        env = "PODMESH_SIDECAR_METADATA_PATH",
        default_value = "/var/run/podmesh/sidecar/metadata.json"
    )]
    metadata_path: String,
    #[arg(long = "metadata-b64", env = "PODMESH_SIDECAR_METADATA_B64")]
    metadata_b64: Option<String>,
    /// Enable transparent egress proxy (requires CAP_NET_ADMIN for nftables)
    #[arg(
        long = "enable-egress",
        env = "PODMESH_ENABLE_EGRESS",
        default_value_t = false
    )]
    enable_egress: bool,
    /// Skip nftables programming even when egress is enabled (useful for tests or restricted hosts)
    #[arg(
        long = "skip-egress-nft",
        env = "PODMESH_SKIP_EGRESS_NFT",
        default_value_t = false
    )]
    skip_egress_nft: bool,
    /// Port for HTTP CONNECT proxy (explicit proxy mode, 0 to use default port)
    /// If not specified, HTTP CONNECT proxy is disabled.
    #[arg(long = "http-proxy-port", env = "PODMESH_HTTP_PROXY_PORT")]
    http_proxy_port: Option<u16>,
}

impl TryFrom<Args> for SidecarConfig {
    type Error = anyhow::Error;

    fn try_from(args: Args) -> Result<Self> {
        let metadata = if let Some(blob) = args
            .metadata_b64
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            decode_inline_metadata(blob)?
        } else {
            load_metadata(&args.metadata_path)?.ok_or_else(|| {
                anyhow::anyhow!("sidecar metadata missing at {}", args.metadata_path)
            })?
        };
        metadata
            .validate()
            .context("validate sidecar proxy peers")?;

        let manifest_bytes = crypto::b64_decode(&metadata.manifest_b64)
            .context("failed to decode manifest payload from metadata")?;

        let extraction = extract_sidecar_routes(&manifest_bytes, &metadata.manifest_id)
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
                "no ingress host detected; using fallback manifest id metadata_manifest={} manifest={}",
                metadata.manifest_id, manifest_id
            );
        }
        update_service_route_hosts(&mut routes, &manifest_id);

        Ok(Self {
            identity: podmesh_sidecar::IdentitySource::ephemeral(),
            proxy_peers: metadata.proxy_peers.clone(),
            lookup_interval: Duration::from_secs(args.lookup_interval_secs.max(1)),
            libp2p_host: args.libp2p_host,
            libp2p_port: args.libp2p_port,
            manifest_id,
            ingress_host,
            app_port: DEFAULT_SIDECAR_APP_PORT,
            routes,
            owner_public_key_b64: metadata.owner_public_key_b64.clone(),
            enable_egress: args.enable_egress,
            skip_egress_nft: args.skip_egress_nft,
            http_proxy_port: args.http_proxy_port,
        })
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        error!("podmesh sidecar failed: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    env_logger::init();
    let args = Args::parse();
    let cfg = SidecarConfig::try_from(args)?;
    run_sidecar(cfg).await
}

fn decode_inline_metadata(blob: &str) -> Result<SidecarMetadata> {
    let trimmed = blob.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("inline sidecar metadata blob is empty"));
    }

    let decoded =
        crypto::b64_decode(trimmed).context("failed to decode inline sidecar metadata blob")?;

    let metadata: SidecarMetadata =
        serde_json::from_slice(&decoded).context("failed to parse inline sidecar metadata blob")?;
    Ok(metadata)
}

fn load_metadata(path: &str) -> Result<Option<SidecarMetadata>> {
    let metadata_path = Path::new(path);
    match fs::read(metadata_path) {
        Ok(bytes) => {
            if bytes.is_empty() {
                return Ok(None);
            }
            let metadata: SidecarMetadata = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse sidecar metadata at {}", path))?;
            Ok(Some(metadata))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(anyhow::anyhow!(
            "failed to read sidecar metadata file {}: {}",
            metadata_path.display(),
            err
        )),
    }
}

fn derive_manifest_identity(
    routes: &[SidecarRouteSpec],
    metadata_manifest_id: &str,
) -> (String, String, bool) {
    if let Some((manifest_id, host)) = routes
        .iter()
        .filter(|route| matches!(route.source, SidecarRouteKind::Ingress))
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

fn update_service_route_hosts(routes: &mut [SidecarRouteSpec], manifest_id: &str) {
    for route in routes.iter_mut() {
        if matches!(route.source, SidecarRouteKind::Service) {
            route.host = format!(
                "{}.{}.{}",
                route.service_name, manifest_id, MESH_DOMAIN_SUFFIX
            )
            .to_lowercase();
        }
    }
}
