use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, ensure};
use iroh::{
    Endpoint, EndpointAddr, RelayConfig, RelayMap, RelayMode, SecretKey,
    address_lookup::memory::MemoryLookup, endpoint::presets, tls::CaTlsConfig,
};
use protocol::{
    ENDPOINT_RECORD_VERSION, EndpointRecord, MACHINE_RELAY_GRANT_VERSION,
    MAX_ENDPOINT_DIRECT_ADDRESSES, MachineRelayGrant, MachineRole,
};
use uuid::Uuid;

use super::ValidatedMachineConfig;
use crate::relay::MachineRelayConfig;

#[derive(Clone)]
pub struct SchedulerIdentity {
    transport_secret: SecretKey,
    signing_public: Vec<u8>,
    signing_private: Vec<u8>,
    /// Addressing information for peers learned out of band.
    ///
    /// Peer schedulers are discovered over HTTP rather than through a DNS or
    /// pkarr lookup service, so their relay and direct addresses have to be
    /// seeded here explicitly. Without it gossip knows a peer's EndpointId but
    /// has no way to dial it.
    peer_lookup: MemoryLookup,
}

impl SchedulerIdentity {
    #[cfg(test)]
    pub(crate) fn ephemeral() -> Result<Self> {
        let (signing_public, signing_private) = crypto::ensure_keypair_ephemeral()?;
        Ok(Self {
            transport_secret: SecretKey::generate(),
            signing_public,
            signing_private,
            peer_lookup: MemoryLookup::new(),
        })
    }

    pub fn load(key_dir: &Path) -> Result<Self> {
        let transport_secret = iroh_support::load_or_initialize_iroh_secret(&key_dir.join("iroh"))?;
        crypto::set_keypair_config(crypto::KeypairConfig {
            signing_mode: crypto::KeypairMode::Persistent,
            kem_mode: crypto::KeypairMode::Persistent,
            key_directory: Some(key_dir.join("signing")),
        });
        let (signing_public, signing_private) = crypto::ensure_keypair_on_disk()
            .context("load persistent scheduler application signing key")?;
        Ok(Self {
            transport_secret,
            signing_public,
            signing_private,
            peer_lookup: MemoryLookup::new(),
        })
    }

    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.transport_secret.public()
    }

    /// Address book for peer schedulers discovered over HTTP.
    pub fn peer_lookup(&self) -> MemoryLookup {
        self.peer_lookup.clone()
    }

    #[cfg(test)]
    pub(crate) fn transport_secret(&self) -> SecretKey {
        self.transport_secret.clone()
    }

    pub fn signing_public(&self) -> &[u8] {
        &self.signing_public
    }

    pub fn signing_private(&self) -> &[u8] {
        &self.signing_private
    }

    pub fn issue_relay_grant(
        &self,
        subject: iroh::EndpointId,
        role: MachineRole,
        audience: String,
        now_secs: u64,
    ) -> Result<MachineRelayGrant> {
        MachineRelayGrant {
            version: MACHINE_RELAY_GRANT_VERSION,
            subject_endpoint_id: subject.as_bytes().to_vec(),
            role,
            relay_audience: audience,
            issued_at_secs: now_secs,
            expires_at_secs: now_secs + protocol::MAX_MACHINE_RELAY_GRANT_LIFETIME_SECS,
            token_id: Uuid::new_v4().to_string(),
            issuer_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(&self.signing_public, &self.signing_private, now_secs)
    }

    pub fn endpoint_record(
        &self,
        address: &EndpointAddr,
        now_secs: u64,
        expires_at_secs: u64,
    ) -> Result<EndpointRecord> {
        ensure!(
            address.id == self.endpoint_id(),
            "scheduler reply address does not match persistent transport identity"
        );
        let relay_url = address.relay_urls().next().map(ToString::to_string);
        let direct_addresses = address
            .ip_addrs()
            .take(MAX_ENDPOINT_DIRECT_ADDRESSES)
            .map(ToString::to_string)
            .collect();
        EndpointRecord {
            version: ENDPOINT_RECORD_VERSION,
            endpoint_id: self.endpoint_id().as_bytes().to_vec(),
            relay_url,
            direct_addresses,
            signing_pubkey: String::new(),
            issued_at_secs: now_secs,
            expires_at_secs,
            signature: String::new(),
        }
        .sign(&self.signing_public, &self.signing_private, now_secs)
    }

    pub fn validate_relay_trust(&self, relay: &MachineRelayConfig) -> Result<()> {
        let trusted = relay
            .trusted_issuer_keys
            .iter()
            .map(|key| crypto::b64_decode(key))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            trusted.iter().any(|key| key == &self.signing_public),
            "integrated relay does not trust this scheduler's signing key"
        );
        Ok(())
    }

    pub fn relay_map(&self, config: &ValidatedMachineConfig, now_secs: u64) -> Result<RelayMap> {
        Ok(self.relay_configs(config, now_secs)?.into_iter().collect())
    }

    pub async fn refresh_relay_credentials(
        &self,
        endpoint: &Endpoint,
        config: &ValidatedMachineConfig,
        now_secs: u64,
    ) -> Result<()> {
        ensure!(!endpoint.is_closed(), "scheduler Iroh endpoint is closed");
        for relay in self.relay_configs(config, now_secs)? {
            endpoint
                .insert_relay(relay.url.clone(), Arc::new(relay))
                .await;
        }
        Ok(())
    }

    fn relay_configs(
        &self,
        config: &ValidatedMachineConfig,
        now_secs: u64,
    ) -> Result<Vec<RelayConfig>> {
        let parsed = RelayMap::try_from_iter(config.relay_urls.iter().map(String::as_str))
            .context("parse scheduler machine relay map")?;
        parsed
            .relays::<Vec<_>>()
            .into_iter()
            .map(|relay| {
                let audience = relay.url.to_string();
                let grant = self.issue_relay_grant(
                    self.endpoint_id(),
                    MachineRole::Scheduler,
                    audience,
                    now_secs,
                )?;
                let token = grant.to_auth_token(now_secs)?;
                Ok::<RelayConfig, anyhow::Error>(relay.as_ref().clone().with_auth_token(token))
            })
            .collect::<Result<Vec<_>>>()
    }

    pub async fn bind_endpoint(
        &self,
        config: &ValidatedMachineConfig,
        now_secs: u64,
    ) -> Result<Endpoint> {
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(self.transport_secret.clone())
            .relay_mode(RelayMode::Custom(self.relay_map(config, now_secs)?))
            .address_lookup(self.peer_lookup.clone())
            .bind_addr(config.bind_addr)?;
        if !config.relay_ca_certificates.is_empty() {
            builder = builder.ca_tls_config(
                CaTlsConfig::embedded().with_extra_roots(config.relay_ca_certificates.clone()),
            );
        }
        builder.bind().await.context("bind scheduler Iroh endpoint")
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::{collections::HashSet, time::Duration};

    use super::*;

    #[test]
    fn scheduler_identity_is_stable_across_restart() {
        let temp = tempfile::tempdir().unwrap();
        let first = SchedulerIdentity::load(temp.path()).unwrap();
        let second = SchedulerIdentity::load(temp.path()).unwrap();
        assert_eq!(first.endpoint_id(), second.endpoint_id());
        assert_eq!(first.signing_public(), second.signing_public());
        assert_eq!(
            std::fs::metadata(temp.path().join("iroh/iroh_secret.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn relay_credentials_are_short_lived_and_refreshable() {
        const NOW: u64 = 10_000;
        let identity = SchedulerIdentity::ephemeral().unwrap();
        let config = ValidatedMachineConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            relay_urls: vec!["https://relay.example.test".into()],
            relay_ca_certificates: Vec::new(),
            scheduler_members: HashSet::from([identity.endpoint_id()]),
            scheduler_bootstraps: Vec::new(),
            query_timeout: Duration::from_secs(5),
            max_pending_queries: 8,
            max_seen_queries: 16,
            max_attached_agents: 8,
            max_offers_per_query: 8,
            max_agent_fanout: 8,
        };
        let first = identity.relay_configs(&config, NOW).unwrap().remove(0);
        let refreshed = identity
            .relay_configs(&config, NOW + 120)
            .unwrap()
            .remove(0);
        assert_ne!(first.auth_token, refreshed.auth_token);
        let token = refreshed.auth_token.unwrap();
        let grant = MachineRelayGrant::from_auth_token(&token, NOW + 120).unwrap();
        grant
            .verify(
                &[identity.signing_public().to_vec()],
                identity.endpoint_id().as_bytes(),
                "https://relay.example.test/",
                NOW + 120,
            )
            .unwrap();
    }
}
