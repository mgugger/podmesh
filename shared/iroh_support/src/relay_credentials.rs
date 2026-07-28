//! Self-provisioning relay credentials.
//!
//! An `iroh-relay` needs a TLS keypair and, for the workload relay, a shared
//! access token. Requiring an operator to mint those by hand before anything
//! starts makes a first run impossible without out-of-band setup, so podmesh
//! generates them on first start and persists them next to the node's other
//! keys. An operator who wants their own certificate authority still supplies
//! explicit paths and podmesh leaves them untouched.
//!
//! Generated certificates are self-signed. Peers do not trust them through the
//! public web PKI: the certificate is pinned out of band, distributed to
//! sidecars inside the owner-encrypted execution specification, and to
//! schedulers and agents as an explicitly configured CA file.

use std::{
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rand::RngCore;

const CREDENTIAL_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const PUBLIC_FILE_MODE: u32 = 0o644;
const CERTIFICATE_FILE: &str = "relay.crt";
const PRIVATE_KEY_FILE: &str = "relay.key";
const AUTH_TOKEN_FILE: &str = "auth-token";
/// 32 random bytes, base64url encoded. Comfortably above the relay's 32-byte
/// minimum token length while staying ASCII and free of whitespace.
const AUTH_TOKEN_ENTROPY_BYTES: usize = 32;
/// Bounds the subject alternative names baked into a generated certificate so a
/// misconfigured relay URL cannot produce an unbounded certificate.
const MAX_SUBJECT_ALT_NAMES: usize = 8;

/// Paths to a relay's TLS material plus the certificate in DER form, which is
/// what peers pin.
#[derive(Debug, Clone)]
pub struct RelayTlsMaterial {
    pub certificate_path: PathBuf,
    pub private_key_path: PathBuf,
    pub certificate_der: Vec<u8>,
}

/// Returns the relay's TLS material, generating a self-signed certificate in
/// `directory` when the operator did not supply one.
///
/// Passing both `certificate_override` and `private_key_override` uses the
/// operator's files verbatim. Supplying only one is a configuration error: a
/// certificate without its key cannot serve TLS, and silently generating the
/// missing half would serve a certificate the operator never approved.
pub fn ensure_relay_tls(
    directory: &Path,
    relay_url: &str,
    certificate_override: Option<PathBuf>,
    private_key_override: Option<PathBuf>,
) -> Result<RelayTlsMaterial> {
    match (certificate_override, private_key_override) {
        (Some(certificate_path), Some(private_key_path)) => {
            let certificate_der = read_first_certificate_der(&certificate_path)?;
            Ok(RelayTlsMaterial {
                certificate_path,
                private_key_path,
                certificate_der,
            })
        }
        (None, None) => generate_or_load_relay_tls(directory, relay_url),
        _ => anyhow::bail!(
            "relay TLS certificate and private key must be configured together; \
             omit both to let podmesh generate a self-signed pair"
        ),
    }
}

/// Returns the relay's shared access token, generating a random one in
/// `directory` when the operator did not supply theirs.
pub fn ensure_relay_auth_token(directory: &Path, token_override: Option<String>) -> Result<String> {
    if let Some(token) = token_override {
        return Ok(token);
    }
    ensure_credential_directory(directory)?;
    let path = directory.join(AUTH_TOKEN_FILE);
    if path.exists() {
        let token = fs::read_to_string(&path)
            .with_context(|| format!("read relay auth token {}", path.display()))?;
        let token = token.trim().to_string();
        ensure!(
            !token.is_empty(),
            "persisted relay auth token {} is empty",
            path.display()
        );
        return Ok(token);
    }

    let mut entropy = [0u8; AUTH_TOKEN_ENTROPY_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut entropy);
    let token = base64_url(&entropy);
    write_private(&path, token.as_bytes())?;
    log::info!(
        "generated a relay auth token at {} because none was configured",
        path.display()
    );
    Ok(token)
}

fn generate_or_load_relay_tls(directory: &Path, relay_url: &str) -> Result<RelayTlsMaterial> {
    ensure_credential_directory(directory)?;
    let certificate_path = directory.join(CERTIFICATE_FILE);
    let private_key_path = directory.join(PRIVATE_KEY_FILE);

    if certificate_path.exists() && private_key_path.exists() {
        let certificate_der = read_first_certificate_der(&certificate_path)?;
        return Ok(RelayTlsMaterial {
            certificate_path,
            private_key_path,
            certificate_der,
        });
    }
    ensure!(
        !certificate_path.exists() && !private_key_path.exists(),
        "relay TLS state in {} is half written; remove {CERTIFICATE_FILE} and {PRIVATE_KEY_FILE} and restart",
        directory.display()
    );

    let subject_alt_names = subject_alt_names(relay_url)?;
    let certified = rcgen::generate_simple_self_signed(subject_alt_names.clone())
        .context("generate self-signed relay certificate")?;
    write_public(&certificate_path, certified.cert.pem().as_bytes())?;
    write_private(
        &private_key_path,
        certified.signing_key.serialize_pem().as_bytes(),
    )?;
    log::info!(
        "generated a self-signed relay certificate at {} for {:?} because none was configured",
        certificate_path.display(),
        subject_alt_names
    );
    Ok(RelayTlsMaterial {
        certificate_path,
        private_key_path,
        certificate_der: certified.cert.der().to_vec(),
    })
}

/// Derives the names the certificate must be valid for from the relay URL that
/// peers will dial, always including loopback so a single-host deployment
/// works without DNS.
fn subject_alt_names(relay_url: &str) -> Result<Vec<String>> {
    let host = relay_url
        .split("://")
        .nth(1)
        .unwrap_or(relay_url)
        .split('/')
        .next()
        .unwrap_or_default()
        .rsplit_once(':')
        .map(|(host, _port)| host)
        .unwrap_or_else(|| {
            relay_url
                .split("://")
                .nth(1)
                .unwrap_or(relay_url)
                .split('/')
                .next()
                .unwrap_or_default()
        })
        .trim_matches(|c| c == '[' || c == ']');
    ensure!(!host.is_empty(), "relay URL {relay_url} has no host");

    let mut names = vec![host.to_string()];
    for fallback in ["localhost", "127.0.0.1", "::1"] {
        if !names.iter().any(|name| name == fallback) {
            names.push(fallback.to_string());
        }
    }
    ensure!(
        names.len() <= MAX_SUBJECT_ALT_NAMES,
        "relay certificate would carry too many subject alternative names"
    );
    Ok(names)
}

fn read_first_certificate_der(path: &Path) -> Result<Vec<u8>> {
    use rustls_pki_types::{CertificateDer, pem::PemObject};
    let certificate = CertificateDer::from_pem_file(path)
        .with_context(|| format!("parse relay TLS certificate {}", path.display()))?;
    Ok(certificate.as_ref().to_vec())
}

fn ensure_credential_directory(directory: &Path) -> Result<()> {
    if directory.exists() {
        let metadata = fs::symlink_metadata(directory)
            .with_context(|| format!("inspect {}", directory.display()))?;
        ensure!(
            metadata.file_type().is_dir(),
            "relay credential path {} is not a directory",
            directory.display()
        );
        return Ok(());
    }
    fs::create_dir_all(directory)
        .with_context(|| format!("create relay credential directory {}", directory.display()))?;
    fs::set_permissions(
        directory,
        fs::Permissions::from_mode(CREDENTIAL_DIRECTORY_MODE),
    )
    .with_context(|| format!("secure relay credential directory {}", directory.display()))?;
    Ok(())
}

fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    write_with_mode(path, contents, PRIVATE_FILE_MODE)
}

fn write_public(path: &Path, contents: &[u8]) -> Result<()> {
    write_with_mode(path, contents, PUBLIC_FILE_MODE)
}

fn write_with_mode(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    use std::io::Write as _;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("flush {}", path.display()))?;
    Ok(())
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tls_is_reused_across_restarts() {
        let temp = tempfile::tempdir().unwrap();
        let first = ensure_relay_tls(temp.path(), "https://proxy:7443", None, None).unwrap();
        let second = ensure_relay_tls(temp.path(), "https://proxy:7443", None, None).unwrap();
        assert_eq!(first.certificate_der, second.certificate_der);
        assert_eq!(
            fs::metadata(&first.private_key_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            PRIVATE_FILE_MODE
        );
    }

    #[test]
    fn generated_token_is_reused_and_long_enough() {
        let temp = tempfile::tempdir().unwrap();
        let first = ensure_relay_auth_token(temp.path(), None).unwrap();
        let second = ensure_relay_auth_token(temp.path(), None).unwrap();
        assert_eq!(first, second);
        assert!(first.len() >= 32, "token must satisfy the relay minimum");
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii() && !b.is_ascii_whitespace() && !b.is_ascii_control())
        );
    }

    #[test]
    fn an_explicit_token_is_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let explicit = "operator-supplied-token-0123456789".to_string();
        let resolved = ensure_relay_auth_token(temp.path(), Some(explicit.clone())).unwrap();
        assert_eq!(resolved, explicit);
        assert!(!temp.path().join(AUTH_TOKEN_FILE).exists());
    }

    #[test]
    fn a_half_configured_tls_pair_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let error = ensure_relay_tls(
            temp.path(),
            "https://proxy:7443",
            Some(temp.path().join("only.crt")),
            None,
        )
        .unwrap_err();
        assert!(format!("{error}").contains("must be configured together"));
    }

    #[test]
    fn subject_alt_names_cover_the_relay_host_and_loopback() {
        assert_eq!(
            subject_alt_names("https://proxy:7443").unwrap(),
            vec!["proxy", "localhost", "127.0.0.1", "::1"]
        );
        assert_eq!(
            subject_alt_names("https://localhost:7443").unwrap(),
            vec!["localhost", "127.0.0.1", "::1"]
        );
    }
}
