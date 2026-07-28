mod access;
mod config;
mod tls;

use std::sync::Arc;

use anyhow::{Result, anyhow};
use iroh_relay::server::{
    Access, AccessControl, ClientRequest, QuicConfig, RelayConfig, Server, ServerConfig, TlsConfig,
};

pub use access::MachineRelayAccessControl;
pub use config::{CertificateMode, MAX_TRUSTED_RELAY_ISSUERS, MachineRelayConfig};

#[derive(Debug)]
pub struct DenyAll;

impl AccessControl for DenyAll {
    async fn on_connect(&self, _request: &ClientRequest) -> Access {
        Access::Deny {
            reason: Some("machine relay grants are required".into()),
        }
    }
}

pub async fn start<A>(config: MachineRelayConfig, access: A) -> Result<Server>
where
    A: AccessControl,
{
    config.validate()?;
    let certificate = tls::load_certificate(&config)?;
    let mut relay = RelayConfig::new(config.http_listen);
    relay.tls = Some(TlsConfig::new(config.https_listen, certificate));
    relay.key_cache_capacity = Some(config.key_cache_capacity);
    relay.access = Arc::new(access);

    let mut server_config = ServerConfig::default();
    server_config.relay = Some(relay);
    server_config.quic = Some(QuicConfig::new(config.qad_listen));
    server_config.metrics_addr = Some(config.metrics_listen);
    Server::spawn(server_config)
        .await
        .map_err(|error| anyhow!("start scheduler machine relay: {error}"))
}
