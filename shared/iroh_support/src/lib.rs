use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, ensure};
use iroh::{EndpointAddr, EndpointId, SecretKey};
use protocol::EndpointRecord;
use rustls_pki_types::{CertificateDer, pem::PemObject};

mod workload_handshake;
pub use workload_handshake::{
    VerifiedWorkloadHandshake, build_workload_handshake_request, build_workload_handshake_response,
    verify_workload_handshake,
};

pub mod relay_credentials;
pub use relay_credentials::{RelayTlsMaterial, ensure_relay_auth_token, ensure_relay_tls};

pub const IROH_SECRET_FILE: &str = "iroh_secret.key";
pub const IROH_IDENTITY_MARKER_FILE: &str = ".iroh_identity_initialized";
const SECRET_FILE_MODE: u32 = 0o600;
const KEY_DIRECTORY_MODE: u32 = 0o700;
const SECRET_KEY_BYTES: usize = 32;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MAX_CA_CERTIFICATE_FILES: usize = 8;
const MAX_CA_CERTIFICATE_FILE_BYTES: u64 = 1024 * 1024;
const MAX_CA_CERTIFICATES: usize = 32;

pub fn load_ca_certificates(paths: &[PathBuf]) -> Result<Vec<CertificateDer<'static>>> {
    ensure!(
        paths.len() <= MAX_CA_CERTIFICATE_FILES,
        "too many CA certificate files"
    );
    let mut certificates = Vec::new();
    for path in paths {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect CA certificate file {}", path.display()))?;
        ensure!(
            metadata.file_type().is_file(),
            "CA certificate path must be a regular file"
        );
        ensure!(
            metadata.len() > 0 && metadata.len() <= MAX_CA_CERTIFICATE_FILE_BYTES,
            "CA certificate file size is outside its bound"
        );
        let parsed = CertificateDer::pem_file_iter(path)
            .with_context(|| format!("open CA certificate file {}", path.display()))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("parse CA certificate file {}", path.display()))?;
        ensure!(
            !parsed.is_empty(),
            "CA certificate file contains no certificates"
        );
        certificates.extend(parsed);
        ensure!(
            certificates.len() <= MAX_CA_CERTIFICATES,
            "too many CA certificates"
        );
    }
    Ok(certificates)
}

pub fn endpoint_addr(record: &EndpointRecord, now_secs: u64) -> Result<EndpointAddr> {
    record.verify(now_secs)?;
    let endpoint_bytes: [u8; protocol::IROH_ENDPOINT_ID_BYTES] = record
        .endpoint_id
        .as_slice()
        .try_into()
        .context("endpoint record ID length is invalid")?;
    let endpoint_id = EndpointId::from_bytes(&endpoint_bytes)
        .context("endpoint record contains an invalid EndpointId")?;
    let mut address = EndpointAddr::new(endpoint_id);
    if let Some(relay_url) = &record.relay_url {
        address = address.with_relay_url(relay_url.parse().context("invalid endpoint relay URL")?);
    }
    for direct in &record.direct_addresses {
        address = address.with_ip_addr(direct.parse().context("invalid endpoint direct address")?);
    }
    Ok(address)
}

pub fn load_or_initialize_iroh_secret(key_dir: &Path) -> Result<SecretKey> {
    ensure_key_directory(key_dir)?;
    let key_path = key_dir.join(IROH_SECRET_FILE);
    let marker_path = key_dir.join(IROH_IDENTITY_MARKER_FILE);

    if key_path.exists() {
        let secret = read_secret(&key_path)?;
        ensure_marker(&marker_path)?;
        return Ok(secret);
    }

    if marker_path.exists() {
        ensure!(
            key_path.exists(),
            "initialized Iroh identity is missing its secret key"
        );
        let secret = read_secret(&key_path)?;
        validate_private_file(&marker_path)?;
        return Ok(secret);
    }

    let secret = SecretKey::generate();
    let created = atomic_create(&key_path, &secret.to_bytes())?;
    ensure_marker(&marker_path)?;
    if created {
        Ok(secret)
    } else {
        read_secret(&key_path)
    }
}

fn ensure_key_directory(key_dir: &Path) -> Result<()> {
    if key_dir.exists() {
        let metadata = fs::symlink_metadata(key_dir)?;
        ensure!(
            metadata.file_type().is_dir(),
            "Iroh key path is not a directory"
        );
        let mode = metadata.permissions().mode() & 0o777;
        ensure!(
            mode == KEY_DIRECTORY_MODE,
            "Iroh key directory permissions must be 0700"
        );
        return Ok(());
    }
    fs::create_dir_all(key_dir)
        .with_context(|| format!("create Iroh key directory {}", key_dir.display()))?;
    fs::set_permissions(key_dir, fs::Permissions::from_mode(KEY_DIRECTORY_MODE))
        .with_context(|| format!("secure Iroh key directory {}", key_dir.display()))?;
    Ok(())
}

fn validate_private_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect Iroh identity file {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "Iroh identity path is not a regular file"
    );
    let mode = metadata.permissions().mode() & 0o777;
    ensure!(
        mode == SECRET_FILE_MODE,
        "Iroh identity file permissions must be 0600"
    );
    Ok(())
}

fn read_secret(path: &Path) -> Result<SecretKey> {
    validate_private_file(path)?;
    ensure!(
        fs::metadata(path)?.len() == SECRET_KEY_BYTES as u64,
        "Iroh secret key must contain exactly 32 bytes"
    );
    let bytes =
        fs::read(path).with_context(|| format!("read Iroh secret key {}", path.display()))?;
    ensure!(
        bytes.len() == SECRET_KEY_BYTES,
        "Iroh secret key must contain exactly 32 bytes"
    );
    let secret_bytes: [u8; SECRET_KEY_BYTES] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid Iroh secret key length"))?;
    Ok(SecretKey::from_bytes(&secret_bytes))
}

fn ensure_marker(path: &Path) -> Result<()> {
    if !atomic_create(path, b"initialized\n")? {
        validate_private_file(path)?;
    }
    Ok(())
}

fn atomic_create(path: &Path, contents: &[u8]) -> Result<bool> {
    let temp_path = temporary_path(path)?;
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(SECRET_FILE_MODE)
            .open(&temp_path)
            .with_context(|| format!("create temporary identity file {}", temp_path.display()))?;
        file.write_all(contents)
            .with_context(|| format!("write temporary identity file {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temporary identity file {}", temp_path.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
        return write_result.map(|()| false);
    }

    let created = match fs::hard_link(&temp_path, path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            return Err(error).with_context(|| {
                format!(
                    "atomically commit identity file {} to {}",
                    temp_path.display(),
                    path.display()
                )
            });
        }
    };
    fs::remove_file(&temp_path)
        .with_context(|| format!("remove temporary identity file {}", temp_path.display()))?;
    if created {
        sync_parent(path)?;
    }
    Ok(created)
}

fn temporary_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("Iroh identity path has invalid file name"))?;
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(path.with_file_name(format!(
        ".{file_name}.tmp-{}-{sequence}",
        std::process::id()
    )))
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Iroh identity path has no parent"))?;
    let directory = OpenOptions::new()
        .read(true)
        .open(parent)
        .with_context(|| format!("open Iroh identity directory {}", parent.display()))?;
    directory
        .sync_all()
        .with_context(|| format!("sync Iroh identity directory {}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn identity_is_stable_across_restarts() {
        let temp = tempfile::tempdir().unwrap();
        let key_dir = temp.path().join("keys");
        let first = load_or_initialize_iroh_secret(&key_dir).unwrap();
        let second = load_or_initialize_iroh_secret(&key_dir).unwrap();
        assert_eq!(first.public(), second.public());
        assert_eq!(
            fs::metadata(key_dir.join(IROH_SECRET_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            SECRET_FILE_MODE
        );
        assert_eq!(
            fs::metadata(&key_dir).unwrap().permissions().mode() & 0o777,
            KEY_DIRECTORY_MODE
        );
    }

    #[test]
    fn initialized_identity_cannot_silently_rotate() {
        let temp = tempfile::tempdir().unwrap();
        let key_dir = temp.path().join("keys");
        load_or_initialize_iroh_secret(&key_dir).unwrap();
        fs::remove_file(key_dir.join(IROH_SECRET_FILE)).unwrap();
        assert!(load_or_initialize_iroh_secret(&key_dir).is_err());
    }

    #[test]
    fn malformed_or_broadly_readable_secret_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let key_dir = temp.path().join("keys");
        load_or_initialize_iroh_secret(&key_dir).unwrap();
        let key_path = key_dir.join(IROH_SECRET_FILE);
        fs::write(&key_path, [1; 31]).unwrap();
        assert!(load_or_initialize_iroh_secret(&key_dir).is_err());

        fs::write(&key_path, [1; 32]).unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_or_initialize_iroh_secret(&key_dir).is_err());
    }

    #[test]
    fn broadly_accessible_existing_directory_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let key_dir = temp.path().join("keys");
        fs::create_dir(&key_dir).unwrap();
        fs::set_permissions(&key_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(load_or_initialize_iroh_secret(&key_dir).is_err());
    }

    #[test]
    fn concurrent_initialization_creates_one_identity() {
        const INITIALIZER_COUNT: usize = 16;
        let temp = tempfile::tempdir().unwrap();
        let key_dir = Arc::new(temp.path().join("keys"));
        fs::create_dir(&*key_dir).unwrap();
        fs::set_permissions(&*key_dir, fs::Permissions::from_mode(KEY_DIRECTORY_MODE)).unwrap();
        let barrier = Arc::new(Barrier::new(INITIALIZER_COUNT));
        let handles: Vec<_> = (0..INITIALIZER_COUNT)
            .map(|_| {
                let key_dir = Arc::clone(&key_dir);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    load_or_initialize_iroh_secret(&key_dir).unwrap().public()
                })
            })
            .collect();
        let identities: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(identities.iter().all(|identity| *identity == identities[0]));
    }
}
