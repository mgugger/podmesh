use std::{fs, io::ErrorKind, net::SocketAddr, path::Path, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use log::error;

use podmesh_sidecar::{
    DEFAULT_SIDECAR_APP_PORT, SidecarConfig, manifest_routes::extract_sidecar_routes, run_sidecar,
};
use protocol::{
    MESH_DOMAIN_SUFFIX,
    machine::{SidecarRouteKind, SidecarRouteSpec},
    sidecar_metadata::SidecarMetadata,
};

#[derive(Parser, Debug)]
#[command(name = "podmesh-sidecar", author, version, about = " Podmesh sidecar")]
struct Args {
    #[arg(long, env = "lookup_interval_secs", default_value_t = 15)]
    lookup_interval_secs: u64,
    #[arg(
        long = "iroh-bind",
        env = "PODMESH_SIDECAR_IROH_BIND",
        default_value = "0.0.0.0:0"
    )]
    iroh_bind_addr: SocketAddr,
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
        metadata.validate().context("validate sidecar metadata")?;

        let manifest_bytes = crypto::b64_decode(&metadata.manifest_b64)
            .context("failed to decode manifest payload from metadata")?;

        // The routing key is not the sidecar's to invent: it is derived from
        // the owner key and workload name and validated above, so a sidecar
        // cannot register under another tenant's key.
        let manifest_id = metadata.manifest_id.clone();
        let extraction = extract_sidecar_routes(&manifest_bytes, &manifest_id)
            .with_context(|| format!("failed to extract routes for manifest {manifest_id}"))?;

        let mut routes = extraction.routes;
        // Every workload is always reachable under its own routing key, whether
        // or not the manifest declares an Ingress.
        let ingress_host = format!("{manifest_id}.{MESH_DOMAIN_SUFFIX}");
        routes.push(SidecarRouteSpec {
            host: ingress_host.clone(),
            path_prefix: "/".to_string(),
            target_port: DEFAULT_SIDECAR_APP_PORT,
            service_name: metadata.workload_name.clone(),
            service_port: DEFAULT_SIDECAR_APP_PORT.to_string(),
            source: SidecarRouteKind::Ingress,
        });

        Ok(Self {
            identity: podmesh_sidecar::IdentitySource::ephemeral(),
            proxy_endpoints: metadata.proxy_endpoints.clone(),
            workload_relay_auth_token: Some(metadata.workload_relay_auth_token.clone()),
            workload_relay_ca_certificates: metadata.workload_relay_ca_certificates.clone(),
            lookup_interval: Duration::from_secs(args.lookup_interval_secs.max(1)),
            iroh_bind_addr: args.iroh_bind_addr,
            workload_name: metadata.workload_name.clone(),
            replica_index: metadata.replica_index,
            replica_count: metadata.replica_count,
            manifest_id,
            ingress_host,
            app_port: DEFAULT_SIDECAR_APP_PORT,
            routes,
            owner_public_key_b64: Some(metadata.owner_public_key_b64.clone()),
            workload_credential_b64: Some(metadata.workload_credential_b64.clone()),
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
