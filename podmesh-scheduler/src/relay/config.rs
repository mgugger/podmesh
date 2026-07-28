use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Result, ensure};
use clap::{Args, ValueEnum};
use iroh_relay::RelayMap;

pub const DEFAULT_HTTP_LISTEN: &str = "0.0.0.0:80";
pub const DEFAULT_HTTPS_LISTEN: &str = "0.0.0.0:443";
pub const DEFAULT_QAD_LISTEN: &str = "0.0.0.0:7842";
pub const DEFAULT_METRICS_LISTEN: &str = "0.0.0.0:9090";
pub const DEFAULT_KEY_CACHE_CAPACITY: usize = 16_384;
pub const MAX_ACME_DOMAINS: usize = 16;
pub const MAX_DNS_NAME_BYTES: usize = 253;
pub const MAX_CONTACT_BYTES: usize = 320;
pub const MAX_TRUSTED_RELAY_ISSUERS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CertificateMode {
    Manual,
    Acme,
}

#[derive(Clone, Debug, Args)]
pub struct MachineRelayConfig {
    #[arg(long = "relay-audience", env = "PODMESH_RELAY_AUDIENCE")]
    pub audience: String,

    #[arg(
        long = "relay-trusted-issuer-key",
        env = "PODMESH_RELAY_TRUSTED_ISSUER_KEYS",
        value_delimiter = ','
    )]
    pub trusted_issuer_keys: Vec<String>,

    #[arg(long = "relay-http-listen", env = "PODMESH_RELAY_HTTP_LISTEN", default_value = DEFAULT_HTTP_LISTEN)]
    pub http_listen: SocketAddr,

    #[arg(long = "relay-https-listen", env = "PODMESH_RELAY_HTTPS_LISTEN", default_value = DEFAULT_HTTPS_LISTEN)]
    pub https_listen: SocketAddr,

    #[arg(long = "relay-qad-listen", env = "PODMESH_RELAY_QAD_LISTEN", default_value = DEFAULT_QAD_LISTEN)]
    pub qad_listen: SocketAddr,

    #[arg(long = "relay-metrics-listen", env = "PODMESH_RELAY_METRICS_LISTEN", default_value = DEFAULT_METRICS_LISTEN)]
    pub metrics_listen: SocketAddr,

    #[arg(
        long = "relay-certificate-mode",
        env = "PODMESH_RELAY_CERTIFICATE_MODE",
        value_enum,
        default_value = "manual"
    )]
    pub certificate_mode: CertificateMode,

    #[arg(long = "relay-tls-certificate", env = "PODMESH_RELAY_TLS_CERTIFICATE")]
    pub tls_certificate: Option<PathBuf>,

    #[arg(long = "relay-tls-private-key", env = "PODMESH_RELAY_TLS_PRIVATE_KEY")]
    pub tls_private_key: Option<PathBuf>,

    #[arg(
        long = "relay-acme-domain",
        env = "PODMESH_RELAY_ACME_DOMAIN",
        value_delimiter = ','
    )]
    pub acme_domains: Vec<String>,

    #[arg(long = "relay-acme-contact", env = "PODMESH_RELAY_ACME_CONTACT")]
    pub acme_contact: Option<String>,

    #[arg(
        long = "relay-acme-cache-dir",
        env = "PODMESH_RELAY_ACME_CACHE_DIR",
        default_value = "/var/lib/podmesh-scheduler/acme"
    )]
    pub acme_cache_dir: PathBuf,

    #[arg(
        long = "relay-acme-staging",
        env = "PODMESH_RELAY_ACME_STAGING",
        default_value_t = false
    )]
    pub acme_staging: bool,

    #[arg(
        long = "relay-key-cache-capacity",
        env = "PODMESH_RELAY_KEY_CACHE_CAPACITY",
        default_value_t = DEFAULT_KEY_CACHE_CAPACITY
    )]
    pub key_cache_capacity: usize,
}

impl MachineRelayConfig {
    pub fn canonical_audience(&self) -> Result<String> {
        let map = RelayMap::try_from_iter([self.audience.as_str()])?;
        let audience = map
            .urls::<Vec<_>>()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("relay audience is missing"))?
            .to_string();
        ensure!(
            audience.starts_with("https://"),
            "relay audience must use HTTPS"
        );
        Ok(audience)
    }

    pub fn validate(&self) -> Result<()> {
        self.canonical_audience()?;
        ensure!(
            !self.trusted_issuer_keys.is_empty()
                && self.trusted_issuer_keys.len() <= MAX_TRUSTED_RELAY_ISSUERS,
            "relay requires between 1 and {MAX_TRUSTED_RELAY_ISSUERS} trusted issuer keys"
        );
        for key in &self.trusted_issuer_keys {
            ensure!(
                crypto::b64_decode(key)?.len() == 32,
                "relay trusted issuer key must decode to 32 bytes"
            );
        }
        ensure!(
            !listeners_conflict(self.http_listen, self.https_listen),
            "relay HTTP and HTTPS listeners must differ"
        );
        ensure!(
            !listeners_conflict(self.metrics_listen, self.http_listen)
                && !listeners_conflict(self.metrics_listen, self.https_listen),
            "relay metrics listener must be independent from relay listeners"
        );
        ensure!(
            self.key_cache_capacity > 0,
            "relay key cache capacity must be non-zero"
        );

        match self.certificate_mode {
            CertificateMode::Manual => {
                ensure!(
                    self.tls_certificate.is_some() == self.tls_private_key.is_some(),
                    "manual TLS requires a certificate and a private key together; \
                     omit both to let the scheduler generate a self-signed pair"
                );
                ensure!(
                    self.acme_domains.is_empty(),
                    "manual TLS cannot include ACME domains"
                );
                ensure!(
                    self.acme_contact.is_none(),
                    "manual TLS cannot include an ACME contact"
                );
            }
            CertificateMode::Acme => {
                ensure!(
                    self.tls_certificate.is_none(),
                    "ACME TLS cannot include a certificate path"
                );
                ensure!(
                    self.tls_private_key.is_none(),
                    "ACME TLS cannot include a private key path"
                );
                ensure!(
                    !self.acme_domains.is_empty() && self.acme_domains.len() <= MAX_ACME_DOMAINS,
                    "ACME requires between 1 and {MAX_ACME_DOMAINS} domains"
                );
                ensure!(
                    self.acme_domains
                        .iter()
                        .all(|domain| valid_dns_name(domain)),
                    "invalid ACME domain"
                );
                let contact = self.acme_contact.as_deref().unwrap_or_default();
                ensure!(
                    valid_contact(contact),
                    "ACME requires a valid contact email"
                );
            }
        }
        Ok(())
    }
}

fn listeners_conflict(left: SocketAddr, right: SocketAddr) -> bool {
    left.port() != 0 && right.port() != 0 && left == right
}

fn valid_dns_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_DNS_NAME_BYTES
        && value.is_ascii()
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn valid_contact(value: &str) -> bool {
    value.len() <= MAX_CONTACT_BYTES
        && !value.bytes().any(|byte| byte.is_ascii_whitespace())
        && value
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && valid_dns_name(domain))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mode: CertificateMode) -> MachineRelayConfig {
        MachineRelayConfig {
            audience: "https://relay.example.test".into(),
            trusted_issuer_keys: vec![crypto::b64_encode(&[7; 32])],
            http_listen: "127.0.0.1:10000".parse().unwrap(),
            https_listen: "127.0.0.1:10001".parse().unwrap(),
            qad_listen: "127.0.0.1:10002".parse().unwrap(),
            metrics_listen: "127.0.0.1:10003".parse().unwrap(),
            certificate_mode: mode,
            tls_certificate: None,
            tls_private_key: None,
            acme_domains: Vec::new(),
            acme_contact: None,
            acme_cache_dir: "/tmp/acme".into(),
            acme_staging: true,
            key_cache_capacity: DEFAULT_KEY_CACHE_CAPACITY,
        }
    }

    #[test]
    fn manual_and_acme_inputs_are_mutually_exclusive() {
        let mut manual = config(CertificateMode::Manual);
        manual.tls_certificate = Some("cert.pem".into());
        manual.tls_private_key = Some("key.pem".into());
        assert!(manual.validate().is_ok());
        manual.acme_domains.push("relay.example.com".into());
        assert!(manual.validate().is_err());

        let mut acme = config(CertificateMode::Acme);
        acme.acme_domains.push("relay.example.com".into());
        acme.acme_contact = Some("ops@example.com".into());
        assert!(acme.validate().is_ok());
        acme.tls_certificate = Some("cert.pem".into());
        assert!(acme.validate().is_err());
    }

    #[test]
    fn manual_tls_may_be_fully_self_provisioned_but_never_half_configured() {
        let mut value = config(CertificateMode::Manual);
        assert!(
            value.validate().is_ok(),
            "omitting both paths asks the scheduler to generate its own pair"
        );
        value.tls_certificate = Some("cert.pem".into());
        assert!(
            value.validate().is_err(),
            "a certificate without its key must be rejected"
        );
    }

    #[test]
    fn invalid_operational_bounds_are_rejected() {
        let mut value = config(CertificateMode::Manual);
        value.tls_certificate = Some("cert.pem".into());
        value.tls_private_key = Some("key.pem".into());
        value.metrics_listen = value.https_listen;
        assert!(value.validate().is_err());
        value.metrics_listen = "127.0.0.1:10003".parse().unwrap();
        value.key_cache_capacity = 0;
        assert!(value.validate().is_err());
    }
}
