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
    pub workload_relay_auth_token: Option<String>,
    pub workload_relay_ca_certificates: Vec<Vec<u8>>,
    pub lookup_interval: Duration,
    pub iroh_bind_addr: SocketAddr,
    pub manifest_id: String,
    pub ingress_host: String,
    pub app_port: u16,
    pub routes: Vec<SidecarRouteSpec>,
    pub owner_public_key_b64: Option<String>,
    pub enable_egress: bool,
    pub skip_egress_nft: bool,
    pub http_proxy_port: Option<u16>,
}

impl SidecarConfig {
    pub fn validate(&self) -> Result<()> {
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
        ensure!(!self.manifest_id.is_empty(), "sidecar manifest ID is empty");
        if let Some(owner) = &self.owner_public_key_b64 {
            ensure!(!owner.is_empty(), "sidecar owner public key is empty");
        }
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
    iroh_runtime::run(config, shutdown, event_tx).await
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
        let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
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
            workload_relay_auth_token: None,
            workload_relay_ca_certificates: Vec::new(),
            lookup_interval: Duration::from_secs(1),
            iroh_bind_addr: "127.0.0.1:0".parse().unwrap(),
            manifest_id: "egress-only".into(),
            ingress_host: "egress-only.mesh.local".into(),
            app_port: DEFAULT_SIDECAR_APP_PORT,
            routes: Vec::new(),
            owner_public_key_b64: Some("owner".into()),
            enable_egress: true,
            skip_egress_nft: true,
            http_proxy_port: None,
        };

        config.validate().unwrap();
    }
}
