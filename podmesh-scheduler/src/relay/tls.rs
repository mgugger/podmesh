use std::{fs, os::unix::fs::PermissionsExt, path::Path, sync::Arc};

use anyhow::{Context, Result, ensure};
use iroh_relay::server::{AcmeConfig, CertConfig};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

use super::{CertificateMode, MachineRelayConfig};

const MAX_CERTIFICATE_CHAIN_BYTES: u64 = 1024 * 1024;
const MAX_PRIVATE_KEY_BYTES: u64 = 64 * 1024;
const MAX_CERTIFICATES: usize = 16;
const PRIVATE_FILE_MODE: u32 = 0o600;
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

pub(super) fn load_certificate(config: &MachineRelayConfig) -> Result<CertConfig> {
    let builder = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("configure relay TLS protocol versions")?
    .with_no_client_auth();

    match config.certificate_mode {
        CertificateMode::Manual => {
            let certificate_path = config
                .tls_certificate
                .as_deref()
                .context("manual TLS certificate path is missing")?;
            let private_key_path = config
                .tls_private_key
                .as_deref()
                .context("manual TLS private key path is missing")?;
            validate_regular_file(certificate_path, MAX_CERTIFICATE_CHAIN_BYTES, false)?;
            validate_regular_file(private_key_path, MAX_PRIVATE_KEY_BYTES, true)?;

            let certificates = CertificateDer::pem_file_iter(certificate_path)
                .with_context(|| format!("open TLS certificate {}", certificate_path.display()))?
                .collect::<Result<Vec<_>, _>>()
                .with_context(|| format!("parse TLS certificate {}", certificate_path.display()))?;
            ensure!(
                !certificates.is_empty() && certificates.len() <= MAX_CERTIFICATES,
                "TLS certificate chain must contain between 1 and {MAX_CERTIFICATES} certificates"
            );
            let private_key = PrivateKeyDer::from_pem_file(private_key_path)
                .with_context(|| format!("parse TLS private key {}", private_key_path.display()))?;
            let server_config = builder
                .with_single_cert(certificates, private_key)
                .context("TLS certificate and private key do not form a valid server identity")?;
            Ok(CertConfig::Manual { server_config })
        }
        CertificateMode::Acme => {
            ensure_private_directory(&config.acme_cache_dir)?;
            let contact = config
                .acme_contact
                .as_deref()
                .context("ACME contact is missing")?;
            let acme_config = AcmeConfig::letsencrypt(!config.acme_staging)
                .domains(config.acme_domains.clone())
                .contact(vec![format!("mailto:{contact}")])
                .cache_path(config.acme_cache_dir.clone());
            Ok(CertConfig::LetsEncrypt {
                acme_config,
                server_config_builder: builder,
            })
        }
    }
}

fn validate_regular_file(path: &Path, max_bytes: u64, private: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect TLS file {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "TLS path must be a regular file"
    );
    ensure!(
        metadata.len() > 0 && metadata.len() <= max_bytes,
        "TLS file size is outside its bound"
    );
    if private {
        ensure!(
            metadata.permissions().mode() & 0o777 == PRIVATE_FILE_MODE,
            "TLS private key permissions must be 0600"
        );
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)
            .with_context(|| format!("create ACME cache directory {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
            .with_context(|| format!("secure ACME cache directory {}", path.display()))?;
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect ACME cache directory {}", path.display()))?;
    ensure!(
        metadata.file_type().is_dir(),
        "ACME cache path must be a directory"
    );
    ensure!(
        metadata.permissions().mode() & 0o777 == PRIVATE_DIRECTORY_MODE,
        "ACME cache directory permissions must be 0700"
    );
    Ok(())
}
