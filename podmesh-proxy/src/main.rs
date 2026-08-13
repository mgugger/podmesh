use std::{
    net::SocketAddr,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use clap::Parser;
use log::{error, info};
use tokio::signal;

use podmesh_proxy::{
    Config, IdentitySource, Workload,
    relay::{DEFAULT_RELAY_KEY_CACHE_CAPACITY, WORKLOAD_RELAY_CREDENTIAL_DIR, WorkloadRelayConfig},
};

#[derive(Parser, Debug)]
#[command(name = "podmesh-proxy", author, version, about = "Podmesh proxy")]
struct Args {
    #[arg(
        long = "proxy-endpoint",
        env = "PODMESH_PROXY_ENDPOINTS",
        value_delimiter = ','
    )]
    proxy_endpoint: Vec<String>,
    #[arg(
        long = "key-dir",
        env = "PODMESH_PROXY_KEY_DIR",
        default_value = "/var/lib/podmesh-proxy/keys"
    )]
    key_dir: PathBuf,
    #[arg(long = "init-identity", default_value_t = false)]
    init_identity: bool,
    #[arg(
        long = "iroh-bind",
        env = "PODMESH_PROXY_IROH_BIND",
        default_value = "0.0.0.0:0"
    )]
    iroh_bind_addr: SocketAddr,
    #[arg(long = "workload-relay-url", env = "PODMESH_WORKLOAD_RELAY_URL")]
    workload_relay_url: Option<String>,
    /// Secret every proxy in the mesh shares, from which per-tenant relay
    /// tokens are derived. Generated on first start when omitted.
    #[arg(
        long = "workload-relay-mesh-secret",
        env = "PODMESH_WORKLOAD_RELAY_MESH_SECRET"
    )]
    workload_relay_mesh_secret: Option<String>,

    /// Addresses sidecars should dial instead of the ones this proxy bound.
    ///
    /// Required whenever the proxy is not on the caller's network — behind NAT,
    /// in a container, or simply on another machine — because a bound address
    /// is then not reachable by anyone who needs it.
    #[arg(
        long = "advertise-address",
        env = "PODMESH_PROXY_ADVERTISE_ADDRESSES",
        value_delimiter = ','
    )]
    advertise_addresses: Vec<String>,

    /// Namespace owners this proxy will relay for, or every tenant when empty.
    ///
    /// Relaying is transport: what a workload may register or tunnel is decided
    /// by its owner-signed credential, so an open relay is a reasonable default
    /// and this exists for operators who want to bound whose traffic they carry.
    #[arg(
        long = "relay-tenants",
        env = "PODMESH_PROXY_RELAY_TENANTS",
        value_delimiter = ','
    )]
    relay_tenants: Vec<String>,
    #[arg(
        long = "workload-relay-http-listen",
        env = "PODMESH_WORKLOAD_RELAY_HTTP_LISTEN",
        default_value = "0.0.0.0:80"
    )]
    workload_relay_http_listen: SocketAddr,
    #[arg(
        long = "workload-relay-https-listen",
        env = "PODMESH_WORKLOAD_RELAY_HTTPS_LISTEN",
        default_value = "0.0.0.0:443"
    )]
    workload_relay_https_listen: SocketAddr,
    #[arg(
        long = "workload-relay-qad-listen",
        env = "PODMESH_WORKLOAD_RELAY_QAD_LISTEN",
        default_value = "0.0.0.0:7842"
    )]
    workload_relay_qad_listen: SocketAddr,
    #[arg(
        long = "workload-relay-metrics-listen",
        env = "PODMESH_WORKLOAD_RELAY_METRICS_LISTEN",
        default_value = "0.0.0.0:9091"
    )]
    workload_relay_metrics_listen: SocketAddr,
    #[arg(
        long = "workload-relay-tls-certificate",
        env = "PODMESH_WORKLOAD_RELAY_TLS_CERTIFICATE"
    )]
    workload_relay_tls_certificate: Option<PathBuf>,
    #[arg(
        long = "workload-relay-tls-private-key",
        env = "PODMESH_WORKLOAD_RELAY_TLS_PRIVATE_KEY"
    )]
    workload_relay_tls_private_key: Option<PathBuf>,
    #[arg(
        long = "workload-relay-key-cache-capacity",
        env = "PODMESH_WORKLOAD_RELAY_KEY_CACHE_CAPACITY",
        default_value_t = DEFAULT_RELAY_KEY_CACHE_CAPACITY
    )]
    workload_relay_key_cache_capacity: usize,
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
        long = "enable-ingress",
        env = "enable_ingress",
        default_value_t = false
    )]
    enable_ingress: bool,
    #[arg(long = "owner-pubkey", env = "PODMESH_OWNER_PUBKEY")]
    owner_pubkey: Option<String>,
    /// Serve the workload relay token and certificate from
    /// `GET /api/v1/workload_relay_bootstrap` so `podctl` can configure itself.
    /// Anyone who can reach the REST API can then use this proxy's relay, so
    /// enable it only on a trusted network such as a local development mesh.
    #[arg(
        long = "publish-relay-bootstrap",
        env = "PODMESH_PROXY_PUBLISH_RELAY_BOOTSTRAP",
        default_value_t = false
    )]
    publish_relay_bootstrap: bool,

    /// Requests per minute a single peer address may make against the REST API.
    ///
    /// The grant endpoint is unauthenticated by design, so this is what keeps
    /// submitting grants from being a free way to wedge the bounded grant store.
    /// Zero disables throttling, which is only appropriate where every caller
    /// shares one source address.
    #[arg(
        long = "rest-rate-limit-per-minute",
        env = "PODMESH_PROXY_REST_RATE_LIMIT",
        default_value_t = podmesh_proxy::restapi::DEFAULT_REST_RATE_LIMIT_PER_MINUTE
    )]
    rest_rate_limit_per_minute: u32,

    /// REST URL of a peer proxy to adopt the workload relay token from.
    ///
    /// Ignored when an explicit token is configured. The peer must be started
    /// with `--publish-relay-bootstrap`.
    #[arg(
        long = "workload-relay-bootstrap-url",
        env = "PODMESH_WORKLOAD_RELAY_BOOTSTRAP_URL"
    )]
    workload_relay_bootstrap_url: Option<String>,
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
        proxy_endpoint,
        key_dir,
        init_identity,
        iroh_bind_addr,
        workload_relay_url,
        workload_relay_mesh_secret,
        relay_tenants,
        advertise_addresses,
        workload_relay_http_listen,
        workload_relay_https_listen,
        workload_relay_qad_listen,
        workload_relay_metrics_listen,
        workload_relay_tls_certificate,
        workload_relay_tls_private_key,
        workload_relay_key_cache_capacity,
        rest_host,
        rest_port,
        disable_rest_api,
        enable_ingress,
        owner_pubkey,
        publish_relay_bootstrap,
        rest_rate_limit_per_minute,
        workload_relay_bootstrap_url,
    } = Args::parse();

    if init_identity {
        let identity = IdentitySource::Persistent(key_dir.clone()).load()?;
        info!(
            "proxy identity initialized endpoint_id={} key_dir={}",
            identity.endpoint_id(),
            key_dir.display()
        );
        return Ok(());
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let proxy_endpoints = proxy_endpoint
        .iter()
        .map(|encoded| {
            let bytes = crypto::b64_decode(encoded).context("decode proxy EndpointRecord")?;
            protocol::EndpointRecord::from_bytes(&bytes, now)
        })
        .collect::<Result<Vec<_>>>()?;
    // The relay's TLS pair and access token are provisioned on first start and
    // reused afterwards, so a proxy can be launched without any pre-created
    // secret. Operator-supplied paths and tokens are always honoured verbatim.
    //
    // A sidecar is injected with exactly one workload relay token, so every
    // proxy whose relay it must reach has to accept that same token. The first
    // proxy mints it; the rest adopt it from a peer instead of each minting a
    // token that would lock the others out.
    let relay_url = workload_relay_url.context("--workload-relay-url is required")?;
    let relay_credential_dir = key_dir.join(WORKLOAD_RELAY_CREDENTIAL_DIR);
    let relay_tls = iroh_support::ensure_relay_tls(
        &relay_credential_dir,
        &relay_url,
        workload_relay_tls_certificate,
        workload_relay_tls_private_key,
    )
    .context("provision workload relay TLS material")?;
    let secret_override = match workload_relay_mesh_secret {
        Some(secret) => Some(secret),
        None => match workload_relay_bootstrap_url.as_deref() {
            Some(peer) => Some(fetch_peer_relay_mesh_secret(peer).await?),
            None => None,
        },
    };
    let relay_mesh_secret =
        iroh_support::ensure_relay_mesh_secret(&relay_credential_dir, secret_override)
            .context("provision workload relay mesh secret")?;
    let relay_tenant_policy = if relay_tenants.is_empty() {
        podmesh_proxy::relay::RelayTenantPolicy::All
    } else {
        log::info!(
            "workload relay restricted to {} configured tenants",
            relay_tenants.len()
        );
        podmesh_proxy::relay::RelayTenantPolicy::Only(relay_tenants.into_iter().collect())
    };
    let workload_relay = WorkloadRelayConfig {
        url: relay_url,
        mesh_secret: relay_mesh_secret,
        relay_tenants: relay_tenant_policy,
        http_listen: workload_relay_http_listen,
        https_listen: workload_relay_https_listen,
        qad_listen: workload_relay_qad_listen,
        metrics_listen: workload_relay_metrics_listen,
        tls_certificate: relay_tls.certificate_path,
        tls_private_key: relay_tls.private_key_path,
        key_cache_capacity: workload_relay_key_cache_capacity,
    };

    let mut cfg = Config {
        proxy_endpoints,
        identity: IdentitySource::Persistent(key_dir),
        iroh_bind_addr,
        workload_relay: Some(workload_relay),
        workload_relay_certificate_der: relay_tls.certificate_der,
        publish_relay_bootstrap,
        rest_host,
        rest_port,
        disable_rest_api,
        enable_ingress,
        advertise_addresses,
        rest_rate_limit_per_minute,
        owner_pubkey,
    };
    cfg.apply_defaults();

    let mut workload = Workload::new(cfg)?;
    workload.start().await?;

    if let Some(peer_id) = workload.peer_id() {
        info!("workplane bootstrap node started endpoint_id={}", peer_id);
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

/// Total time allowed for adopting a peer proxy's workload relay token.
const RELAY_BOOTSTRAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Refuses to buffer an oversized relay bootstrap response body.
const MAX_RELAY_BOOTSTRAP_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(serde::Deserialize)]
struct RelayBootstrapResponse {
    mesh_secret: String,
}

/// Adopts a peer proxy's workload relay mesh secret.
///
/// A tenant's relay token is derived from the mesh secret, so every proxy that
/// must validate that tenant's token needs the same secret. Fetching it from a
/// peer keeps a multi-proxy deployment free of hand-copied secrets.
///
/// This is a proxy-to-proxy endpoint. Tenants never receive the secret, only the
/// token derived for them.
async fn fetch_peer_relay_mesh_secret(peer_url: &str) -> Result<String> {
    let base = peer_url.trim().trim_end_matches('/');
    anyhow::ensure!(!base.is_empty(), "empty workload relay bootstrap URL");
    let client = reqwest::Client::builder()
        .timeout(RELAY_BOOTSTRAP_TIMEOUT)
        .build()
        .context("build relay bootstrap HTTP client")?;
    let response = client
        .get(format!("{base}/api/v1/workload_relay_mesh_secret"))
        .send()
        .await
        .with_context(|| format!("GET {base}/api/v1/workload_relay_mesh_secret failed"))?
        .error_for_status()
        .with_context(|| {
            format!("proxy {base} refused to publish its relay mesh secret; start it with --publish-relay-bootstrap")
        })?;
    let body = response
        .bytes()
        .await
        .with_context(|| format!("read relay bootstrap body from {base}"))?;
    anyhow::ensure!(
        body.len() <= MAX_RELAY_BOOTSTRAP_RESPONSE_BYTES,
        "proxy {base} returned an oversized relay bootstrap response"
    );
    let parsed: RelayBootstrapResponse = serde_json::from_slice(&body)
        .with_context(|| format!("decode relay bootstrap response from {base}"))?;
    info!("adopted workload relay mesh secret published by {base}");
    Ok(parsed.mesh_secret)
}
