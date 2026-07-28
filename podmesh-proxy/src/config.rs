use std::{
    net::SocketAddr,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use iroh::SecretKey;
use protocol::EndpointRecord;

use crate::relay::WorkloadRelayConfig;

pub const MAX_CONFIGURED_PROXY_ENDPOINTS: usize = 32;

#[derive(Clone, Debug)]
pub enum IdentitySource {
    Persistent(PathBuf),
    Ephemeral,
}

impl IdentitySource {
    pub fn ephemeral() -> Self {
        Self::Ephemeral
    }

    pub fn load(&self) -> Result<SecretKey> {
        match self {
            Self::Persistent(key_dir) => {
                crypto::set_keypair_config(crypto::KeypairConfig {
                    signing_mode: crypto::KeypairMode::Persistent,
                    kem_mode: crypto::KeypairMode::Persistent,
                    key_directory: Some(key_dir.join("application")),
                });
                crypto::ensure_keypair_on_disk().context("load proxy application signing key")?;
                crypto::ensure_kem_keypair_on_disk().context("load proxy application KEM key")?;
                iroh_support::load_or_initialize_iroh_secret(&key_dir.join("iroh"))
            }
            Self::Ephemeral => Ok(SecretKey::generate()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub proxy_endpoints: Vec<EndpointRecord>,
    pub identity: IdentitySource,
    pub iroh_bind_addr: SocketAddr,
    pub workload_relay: Option<WorkloadRelayConfig>,
    /// DER of the workload relay certificate, published together with the relay
    /// token when `publish_relay_bootstrap` is set.
    pub workload_relay_certificate_der: Vec<u8>,
    /// Serve the relay token and certificate over the REST API so a client can
    /// bootstrap without hand-copied secrets. Trusted networks only.
    pub publish_relay_bootstrap: bool,
    pub rest_host: String,
    pub rest_port: u16,
    pub disable_rest_api: bool,
    pub enable_ingress: bool,
    pub owner_pubkey: Option<String>,
}

impl Config {
    pub fn apply_defaults(&mut self) {
        if self.rest_host.is_empty() {
            self.rest_host = "0.0.0.0".to_string();
        }
        if self.rest_port == 0 {
            self.rest_port = 7100;
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.proxy_endpoints.len() <= MAX_CONFIGURED_PROXY_ENDPOINTS,
            "too many configured proxy endpoints"
        );
        let now = now_secs()?;
        for endpoint in &self.proxy_endpoints {
            endpoint.verify(now)?;
        }
        ensure!(!self.rest_host.is_empty(), "REST host must not be empty");
        if let Some(relay) = &self.workload_relay {
            relay.validate()?;
        }
        Ok(())
    }
}

fn now_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_secs())
}
