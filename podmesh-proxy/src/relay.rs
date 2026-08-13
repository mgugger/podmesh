use std::{
    fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use iroh::{RelayMap, tls::CaTlsConfig};
use iroh_relay::server::{
    Access, AccessControl, CertConfig, ClientRequest, QuicConfig, RelayConfig, Server,
    ServerConfig, TlsConfig,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

const MAX_CERTIFICATE_CHAIN_BYTES: u64 = 1024 * 1024;
const MAX_PRIVATE_KEY_BYTES: u64 = 64 * 1024;
const MAX_CERTIFICATES: usize = 16;
const PRIVATE_FILE_MODE: u32 = 0o600;
pub const DEFAULT_RELAY_KEY_CACHE_CAPACITY: usize = 16_384;
/// Sub-directory of the proxy key directory holding the self-provisioned
/// workload relay certificate, private key and access token.
pub const WORKLOAD_RELAY_CREDENTIAL_DIR: &str = "workload-relay";

/// Which tenants a relay admits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RelayTenantPolicy {
    /// Relay for any tenant that presents a valid derived token.
    ///
    /// The default, because relaying is transport: what a workload may register
    /// or tunnel is decided by its owner-signed credential, not by this.
    #[default]
    All,
    /// Relay only for the listed namespace owners.
    Only(std::collections::HashSet<String>),
}

impl RelayTenantPolicy {
    fn admits(&self, owner_pubkey_b64: &str) -> bool {
        // Mesh components are not tenants, and restricting which tenants a
        // relay serves must not stop the proxy reaching its own relay.
        if owner_pubkey_b64 == protocol::relay_token::MESH_COMPONENT_TENANT {
            return true;
        }
        match self {
            Self::All => true,
            Self::Only(owners) => owners.contains(owner_pubkey_b64),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkloadRelayConfig {
    pub url: String,
    /// Secret every proxy in the mesh shares, from which each tenant's relay
    /// token is derived. It is never handed to a workload.
    pub mesh_secret: String,
    pub relay_tenants: RelayTenantPolicy,
    pub http_listen: SocketAddr,
    pub https_listen: SocketAddr,
    pub qad_listen: SocketAddr,
    pub metrics_listen: SocketAddr,
    pub tls_certificate: PathBuf,
    pub tls_private_key: PathBuf,
    pub key_cache_capacity: usize,
}

impl WorkloadRelayConfig {
    pub fn validate(&self) -> Result<()> {
        let relay_map = RelayMap::try_from_iter([self.url.as_str()])?;
        let canonical = relay_map
            .urls::<Vec<_>>()
            .into_iter()
            .next()
            .context("workload relay URL is missing")?
            .to_string();
        ensure!(
            canonical.starts_with("https://"),
            "workload relay URL must use HTTPS"
        );
        protocol::relay_token::validate_mesh_secret(&self.mesh_secret)?;
        ensure!(
            !listeners_conflict(self.http_listen, self.https_listen),
            "workload relay HTTP and HTTPS listeners must differ"
        );
        ensure!(
            !listeners_conflict(self.metrics_listen, self.http_listen)
                && !listeners_conflict(self.metrics_listen, self.https_listen),
            "workload relay metrics listener must be independent"
        );
        ensure!(
            self.key_cache_capacity > 0,
            "workload relay key cache capacity must be non-zero"
        );
        validate_regular_file(&self.tls_certificate, MAX_CERTIFICATE_CHAIN_BYTES, false)?;
        validate_regular_file(&self.tls_private_key, MAX_PRIVATE_KEY_BYTES, true)?;
        Ok(())
    }

    pub fn relay_map(&self) -> Result<RelayMap> {
        self.validate()?;
        let parsed = RelayMap::try_from_iter([self.url.as_str()])?;
        let own_token = protocol::derive_tenant_relay_token(
            &self.mesh_secret,
            protocol::relay_token::MESH_COMPONENT_TENANT,
        )?;
        let relays = parsed
            .relays::<Vec<_>>()
            .into_iter()
            .map(|relay| relay.as_ref().clone().with_auth_token(own_token.clone()));
        Ok(relays.collect())
    }

    pub fn ca_tls_config(&self) -> Result<CaTlsConfig> {
        self.validate()?;
        let certificates = CertificateDer::pem_file_iter(&self.tls_certificate)
            .with_context(|| {
                format!(
                    "open workload relay TLS certificate {}",
                    self.tls_certificate.display()
                )
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("parse workload relay TLS certificate")?;
        ensure!(
            !certificates.is_empty() && certificates.len() <= MAX_CERTIFICATES,
            "workload relay TLS chain has an invalid certificate count"
        );
        Ok(CaTlsConfig::embedded().with_extra_roots(certificates))
    }
}

#[derive(Clone, Debug)]
struct WorkloadRelayAccessControl {
    mesh_secret: String,
    tenants: RelayTenantPolicy,
}

impl WorkloadRelayAccessControl {
    fn new(mesh_secret: &str, tenants: RelayTenantPolicy) -> Self {
        Self {
            mesh_secret: mesh_secret.to_string(),
            tenants,
        }
    }

    /// The tenant a request proved, if it may relay here at all.
    fn admitted_tenant(&self, request: &ClientRequest) -> Option<String> {
        let token = request.auth_token()?;
        let owner = protocol::tenant_from_relay_token(&self.mesh_secret, &token).ok()?;
        self.tenants.admits(&owner).then_some(owner)
    }
}

impl AccessControl for WorkloadRelayAccessControl {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        match self.admitted_tenant(request) {
            Some(_owner) => Access::Allow,
            None => {
                // The reason is deliberately the same whether the token was
                // invalid or the tenant simply is not admitted here, so the
                // relay does not disclose which tenants it serves.
                log::warn!(
                    "workload relay denied endpoint {}",
                    request.endpoint_id().fmt_short()
                );
                Access::Deny {
                    reason: Some("invalid workload relay credential".into()),
                }
            }
        }
    }
}

pub async fn start(config: &WorkloadRelayConfig) -> Result<Server> {
    config.validate()?;
    let certificate = load_certificate(config)?;
    let mut relay = RelayConfig::new(config.http_listen);
    relay.tls = Some(TlsConfig::new(config.https_listen, certificate));
    relay.key_cache_capacity = Some(config.key_cache_capacity);
    relay.access = Arc::new(WorkloadRelayAccessControl::new(
        &config.mesh_secret,
        config.relay_tenants.clone(),
    ));

    let mut server_config = ServerConfig::default();
    server_config.relay = Some(relay);
    server_config.quic = Some(QuicConfig::new(config.qad_listen));
    server_config.metrics_addr = Some(config.metrics_listen);
    Server::spawn(server_config)
        .await
        .map_err(|error| anyhow::anyhow!("start proxy workload relay: {error}"))
}

fn load_certificate(config: &WorkloadRelayConfig) -> Result<CertConfig> {
    let certificates = CertificateDer::pem_file_iter(&config.tls_certificate)
        .with_context(|| {
            format!(
                "open workload relay TLS certificate {}",
                config.tls_certificate.display()
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("parse workload relay TLS certificate")?;
    ensure!(
        !certificates.is_empty() && certificates.len() <= MAX_CERTIFICATES,
        "workload relay TLS chain has an invalid certificate count"
    );
    let private_key = PrivateKeyDer::from_pem_file(&config.tls_private_key)
        .context("parse workload relay TLS private key")?;
    let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("configure workload relay TLS versions")?
    .with_no_client_auth()
    .with_single_cert(certificates, private_key)
    .context("workload relay TLS certificate does not match private key")?;
    Ok(CertConfig::Manual { server_config })
}

fn validate_regular_file(path: &Path, max_bytes: u64, private: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect workload relay TLS file {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "relay TLS path must be a regular file"
    );
    ensure!(
        metadata.len() > 0 && metadata.len() <= max_bytes,
        "relay TLS file size is outside its bound"
    );
    if private {
        ensure!(
            metadata.permissions().mode() & 0o777 == PRIVATE_FILE_MODE,
            "relay TLS private key permissions must be 0600"
        );
    }
    Ok(())
}

fn listeners_conflict(left: SocketAddr, right: SocketAddr) -> bool {
    left.port() != 0 && right.port() != 0 && left == right
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(seed: u8) -> String {
        crypto::b64_encode(&[seed; 32])
    }

    #[test]
    fn an_open_relay_admits_every_tenant() {
        let policy = RelayTenantPolicy::All;
        assert!(policy.admits(&owner(1)));
        assert!(policy.admits(&owner(2)));
    }

    /// An operator restricting whose traffic a relay carries must actually
    /// exclude everyone else, or the setting is decorative.
    #[test]
    fn a_restricted_relay_admits_only_its_listed_tenants() {
        let policy = RelayTenantPolicy::Only(std::collections::HashSet::from([owner(1)]));
        assert!(policy.admits(&owner(1)));
        assert!(!policy.admits(&owner(2)));
    }

    /// Restricting tenants must not lock the proxy out of its own relay.
    #[test]
    fn a_restricted_relay_still_admits_mesh_components() {
        let policy = RelayTenantPolicy::Only(std::collections::HashSet::from([owner(1)]));
        assert!(policy.admits(protocol::relay_token::MESH_COMPONENT_TENANT));
    }
}
