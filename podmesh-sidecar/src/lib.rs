use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use log::{info, warn};
use protocol::{EndpointRecord, machine::SidecarRouteSpec};
use tokio::{
    signal,
    sync::{mpsc, oneshot},
};

pub mod egress_nft;
pub mod egress_proxy;
pub mod http_connect_proxy;
mod identity;
mod iroh_runtime;
pub mod manifest_routes;

pub use http_connect_proxy::HTTP_CONNECT_PROXY_PORT;
pub use identity::IdentitySource;

pub const DEFAULT_SIDECAR_APP_PORT: u16 = 18080;
const MAX_PROXY_ENDPOINTS: usize = 32;
const MIN_RELAY_AUTH_TOKEN_BYTES: usize = 32;
const MAX_RELAY_AUTH_TOKEN_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug)]
pub struct SidecarConfig {
    pub identity: IdentitySource,
    pub proxy_endpoints: Vec<EndpointRecord>,
    pub workload_replay_limits: protocol::ReplayLimits,
    pub workload_relay_auth_token: Option<String>,
    pub workload_relay_ca_certificates: Vec<Vec<u8>>,
    pub lookup_interval: Duration,
    pub iroh_bind_addr: SocketAddr,
    pub metrics_listen: Option<SocketAddr>,
    /// Workload name as written in the manifest. Together with the owner key
    /// it derives the routing key the proxy indexes this workload under.
    pub workload_name: String,
    pub manifest_id: String,
    /// Which replica of the deployment this sidecar serves.
    pub replica_index: u32,
    pub replica_count: u32,
    pub ingress_host: String,
    pub app_port: u16,
    pub routes: Vec<SidecarRouteSpec>,
    pub owner_public_key_b64: Option<String>,
    /// Owner-signed proof of tenancy, presented during the proxy handshake.
    pub workload_credential_b64: Option<String>,
    pub enable_egress: bool,
    pub skip_egress_nft: bool,
    pub http_proxy_port: Option<u16>,
}

impl SidecarConfig {
    pub fn validate(&self) -> Result<()> {
        self.workload_replay_limits.validate()?;
        ensure!(
            !self.proxy_endpoints.is_empty() && self.proxy_endpoints.len() <= MAX_PROXY_ENDPOINTS,
            "sidecar proxy endpoint count is invalid"
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock precedes Unix epoch")?
            .as_secs();
        let mut needs_relay_auth = false;
        for record in &self.proxy_endpoints {
            record.verify(now)?;
            needs_relay_auth |= record.relay_url.is_some();
        }
        if needs_relay_auth {
            let token = self
                .workload_relay_auth_token
                .as_ref()
                .context("workload relay auth token is required")?;
            ensure!(
                token.len() >= MIN_RELAY_AUTH_TOKEN_BYTES
                    && token.len() <= MAX_RELAY_AUTH_TOKEN_BYTES,
                "workload relay auth token length is invalid"
            );
            ensure!(
                token.is_ascii()
                    && !token
                        .bytes()
                        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()),
                "workload relay auth token contains invalid characters"
            );
        }
        ensure!(
            self.workload_relay_ca_certificates.len() <= 8,
            "too many workload relay CA certificates"
        );
        for certificate in &self.workload_relay_ca_certificates {
            ensure!(
                !certificate.is_empty() && certificate.len() <= 64 * 1024,
                "invalid workload relay CA certificate size"
            );
        }
        ensure!(
            !self.workload_name.is_empty() && self.workload_name.len() <= 253,
            "sidecar workload name is invalid"
        );
        ensure!(
            self.replica_count >= 1 && self.replica_index < self.replica_count,
            "sidecar replica identity is invalid"
        );
        if let Some(metrics_listen) = self.metrics_listen {
            ensure!(
                !podmesh_metrics::listeners_conflict(metrics_listen, self.iroh_bind_addr),
                "sidecar metrics listener conflicts with the Iroh listener"
            );
            let mut occupied_ports = vec![self.app_port];
            if self.enable_egress {
                occupied_ports.push(crate::egress_proxy::EGRESS_PROXY_PORT);
            }
            if let Some(http_proxy_port) = self.http_proxy_port {
                occupied_ports.push(if http_proxy_port == 0 {
                    crate::HTTP_CONNECT_PROXY_PORT
                } else {
                    http_proxy_port
                });
            }
            ensure!(
                occupied_ports
                    .into_iter()
                    .all(|port| metrics_listen.port() != port),
                "sidecar metrics listener conflicts with a local listener"
            );
        }
        // Without the owner key the sidecar cannot verify a proxy grant, and a
        // sidecar that cannot verify grants would serve tenant traffic to any
        // proxy it can reach.
        let owner = self
            .owner_public_key_b64
            .as_ref()
            .context("sidecar owner public key is required")?;
        ensure!(
            crypto::b64_decode(owner)?.len() == crypto::ED25519_PUBLIC_KEY_SIZE,
            "sidecar owner public key must decode to {} bytes",
            crypto::ED25519_PUBLIC_KEY_SIZE
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarEvent {
    Connected {
        peer_id: String,
    },
    ProxyPeerDiscovered {
        peer_id: String,
    },
    EgressTunnelEstablished {
        dest_host: String,
        dest_port: u16,
    },
    EgressTunnelFailed {
        dest_host: String,
        dest_port: u16,
        error: String,
    },
}

pub async fn run_sidecar(config: SidecarConfig) -> Result<()> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::select! {
            result = signal::ctrl_c() => match result {
                Ok(()) => info!("sidecar received SIGINT"),
                Err(error) => warn!("sidecar SIGINT listener failed: {error}"),
            },
            _ = async {
                #[cfg(unix)]
                {
                    match signal::unix::signal(signal::unix::SignalKind::terminate()) {
                        Ok(mut signal) => { signal.recv().await; }
                        Err(error) => warn!("sidecar SIGTERM listener failed: {error}"),
                    }
                }
                #[cfg(not(unix))]
                std::future::pending::<()>().await;
            } => info!("sidecar received SIGTERM"),
        }
        let _ = shutdown_tx.send(());
    });
    run_sidecar_with_shutdown(config, shutdown_rx, None).await
}

pub async fn run_sidecar_with_shutdown(
    config: SidecarConfig,
    shutdown: oneshot::Receiver<()>,
    event_tx: Option<mpsc::UnboundedSender<SidecarEvent>>,
) -> Result<()> {
    let metrics_runtime = podmesh_metrics::MetricsRuntime::start(
        podmesh_metrics::ComponentName::Sidecar,
        podmesh_metrics::MetricsConfig {
            listen: config.metrics_listen,
        },
        tokio_util::sync::CancellationToken::new(),
    )
    .await?;
    let result =
        iroh_runtime::run_with_metrics(config, shutdown, event_tx, metrics_runtime.metrics()).await;
    let shutdown_result = metrics_runtime.shutdown().await;
    result?;
    shutdown_result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egress_only_sidecar_does_not_require_ingress_routes() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (public, private) = crypto::generate_signing_keypair();
        let endpoint = EndpointRecord {
            version: protocol::ENDPOINT_RECORD_VERSION,
            endpoint_id: iroh::SecretKey::generate().public().as_bytes().to_vec(),
            relay_url: None,
            direct_addresses: vec!["127.0.0.1:4002".into()],
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: now + 60,
            signature: String::new(),
        }
        .sign(&public, &private, now)
        .unwrap();
        let owner = [3u8; crypto::ED25519_PUBLIC_KEY_SIZE];
        let config = SidecarConfig {
            identity: IdentitySource::ephemeral(),
            proxy_endpoints: vec![endpoint],
            workload_replay_limits: protocol::ReplayLimits::default(),
            workload_relay_auth_token: None,
            workload_relay_ca_certificates: Vec::new(),
            lookup_interval: Duration::from_secs(1),
            iroh_bind_addr: "127.0.0.1:0".parse().unwrap(),
            metrics_listen: None,
            workload_name: "egress-only".into(),
            replica_index: 0,
            replica_count: 1,
            manifest_id: protocol::route_id(&owner, "egress-only"),
            ingress_host: "egress-only.mesh.local".into(),
            app_port: DEFAULT_SIDECAR_APP_PORT,
            routes: Vec::new(),
            owner_public_key_b64: Some(crypto::b64_encode(&owner)),
            workload_credential_b64: None,
            enable_egress: true,
            skip_egress_nft: true,
            http_proxy_port: None,
        };

        config.validate().unwrap();
    }

    /// A sidecar without the owner key cannot verify the Biscuit a proxy
    /// presents, so it must refuse to start rather than serve tenant traffic to
    /// whichever proxy happens to reach it.
    #[test]
    fn a_sidecar_without_an_owner_key_is_refused() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (public, private) = crypto::generate_signing_keypair();
        let endpoint = EndpointRecord {
            version: protocol::ENDPOINT_RECORD_VERSION,
            endpoint_id: iroh::SecretKey::generate().public().as_bytes().to_vec(),
            relay_url: None,
            direct_addresses: vec!["127.0.0.1:4002".into()],
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: now + 60,
            signature: String::new(),
        }
        .sign(&public, &private, now)
        .unwrap();
        let config = SidecarConfig {
            identity: IdentitySource::ephemeral(),
            proxy_endpoints: vec![endpoint],
            workload_replay_limits: protocol::ReplayLimits::default(),
            workload_relay_auth_token: None,
            workload_relay_ca_certificates: Vec::new(),
            lookup_interval: Duration::from_secs(1),
            iroh_bind_addr: "127.0.0.1:0".parse().unwrap(),
            metrics_listen: None,
            workload_name: "unowned".into(),
            replica_index: 0,
            replica_count: 1,
            manifest_id: protocol::route_id(&[0u8; 32], "unowned"),
            workload_credential_b64: None,
            ingress_host: "unowned.mesh.local".into(),
            app_port: DEFAULT_SIDECAR_APP_PORT,
            routes: Vec::new(),
            owner_public_key_b64: None,
            enable_egress: false,
            skip_egress_nft: true,
            http_proxy_port: None,
        };

        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("owner public key is required"));
    }
}
