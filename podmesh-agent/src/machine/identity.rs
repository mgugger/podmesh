use std::path::Path;

use anyhow::{Context, Result};
use iroh::{
    Endpoint, SecretKey,
    endpoint::{QuicTransportConfig, VarInt, presets},
    tls::CaTlsConfig,
};

use super::ValidatedMachineConfig;

#[derive(Clone)]
pub struct AgentIdentity {
    secret: SecretKey,
}

impl AgentIdentity {
    pub fn load(key_dir: &Path) -> Result<Self> {
        let secret = iroh_support::load_or_initialize_iroh_secret(&key_dir.join("iroh"))?;
        Ok(Self { secret })
    }

    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.secret.public()
    }

    pub async fn bind(&self, config: &ValidatedMachineConfig) -> Result<Endpoint> {
        let transport = QuicTransportConfig::builder()
            .max_concurrent_uni_streams(VarInt::from_u32(config.max_concurrent_uni_streams))
            .max_concurrent_bidi_streams(VarInt::from_u32(config.max_concurrent_bidi_streams))
            .max_idle_timeout(Some(config.max_idle.try_into()?))
            .stream_receive_window(VarInt::from_u32(config.stream_receive_window_bytes))
            .receive_window(VarInt::from_u32(config.connection_receive_window_bytes))
            .build();
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(self.secret.clone())
            .clear_relay_transports()
            .transport_config(transport)
            .bind_addr(config.bind_addr)?;
        if !config.relay_ca_certificates.is_empty() {
            builder = builder.ca_tls_config(
                CaTlsConfig::embedded().with_extra_roots(config.relay_ca_certificates.clone()),
            );
        }
        builder.bind().await.context("bind agent Iroh endpoint")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_identity_is_stable_across_restart() {
        let temp = tempfile::tempdir().unwrap();
        let first = AgentIdentity::load(temp.path()).unwrap();
        let second = AgentIdentity::load(temp.path()).unwrap();
        assert_eq!(first.endpoint_id(), second.endpoint_id());
    }
}
