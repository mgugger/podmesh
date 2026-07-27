use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use libp2p::identity::Keypair;

pub const LIBP2P_IDENTITY_FILE: &str = "libp2p_identity.key";
const LIBP2P_IDENTITY_MARKER_FILE: &str = ".libp2p_identity_initialized";
const MAX_IDENTITY_FILE_BYTES: usize = 4096;

#[derive(Clone)]
pub enum IdentitySource {
    Persistent(std::path::PathBuf),
    Ephemeral(Keypair),
}

impl std::fmt::Debug for IdentitySource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Persistent(path) => formatter.debug_tuple("Persistent").field(path).finish(),
            Self::Ephemeral(keypair) => formatter
                .debug_tuple("Ephemeral")
                .field(&keypair.public().to_peer_id())
                .finish(),
        }
    }
}

impl IdentitySource {
    pub fn load(&self) -> Result<Keypair> {
        match self {
            Self::Persistent(key_dir) => load_or_initialize_identity(key_dir),
            Self::Ephemeral(keypair) => Ok(keypair.clone()),
        }
    }

    pub fn ephemeral() -> Self {
        Self::Ephemeral(Keypair::generate_ed25519())
    }

    pub fn peer_id(&self) -> Result<libp2p::PeerId> {
        Ok(self.load()?.public().to_peer_id())
    }
}

pub fn load_or_initialize_identity(key_dir: &Path) -> Result<Keypair> {
    ensure_key_dir(key_dir)?;
    let key_path = key_dir.join(LIBP2P_IDENTITY_FILE);
    let marker_path = key_dir.join(LIBP2P_IDENTITY_MARKER_FILE);

    if key_path.exists() {
        return load_identity(&key_path);
    }
    if marker_path.exists() {
        bail!(
            "proxy identity key {} is missing after initialization",
            key_path.display()
        );
    }

    let keypair = Keypair::generate_ed25519();
    let encoded = keypair
        .to_protobuf_encoding()
        .context("encode libp2p identity")?;
    write_new_secret(&key_path, &encoded)?;
    write_new_secret(&marker_path, b"initialized\n")?;
    Ok(keypair)
}

fn load_identity(path: &Path) -> Result<Keypair> {
    validate_secret_permissions(path)?;
    let metadata = fs::metadata(path)
        .with_context(|| format!("read proxy identity metadata {}", path.display()))?;
    if metadata.len() == 0 || metadata.len() > MAX_IDENTITY_FILE_BYTES as u64 {
        bail!("proxy identity file size is invalid");
    }
    let bytes =
        fs::read(path).with_context(|| format!("read proxy identity {}", path.display()))?;
    Keypair::from_protobuf_encoding(&bytes).context("decode proxy libp2p identity")
}

fn ensure_key_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("create proxy key directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_new_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create proxy identity file {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn validate_secret_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!("proxy identity file permissions must not allow group or other access");
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_secret_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_across_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_initialize_identity(dir.path()).unwrap();
        let second = load_or_initialize_identity(dir.path()).unwrap();
        assert_eq!(first.public().to_peer_id(), second.public().to_peer_id());
    }

    #[test]
    fn ephemeral_identity_is_injected_without_disk_state() {
        let source = IdentitySource::ephemeral();
        let first = source.load().unwrap();
        let second = source.load().unwrap();
        assert_eq!(first.public().to_peer_id(), second.public().to_peer_id());
    }

    #[test]
    fn missing_initialized_identity_fails() {
        let dir = tempfile::tempdir().unwrap();
        load_or_initialize_identity(dir.path()).unwrap();
        fs::remove_file(dir.path().join(LIBP2P_IDENTITY_FILE)).unwrap();
        assert!(load_or_initialize_identity(dir.path()).is_err());
    }

    #[test]
    fn malformed_identity_fails() {
        let dir = tempfile::tempdir().unwrap();
        load_or_initialize_identity(dir.path()).unwrap();
        fs::write(dir.path().join(LIBP2P_IDENTITY_FILE), b"invalid").unwrap();
        assert!(load_or_initialize_identity(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn permissive_identity_file_fails() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        load_or_initialize_identity(dir.path()).unwrap();
        let path = dir.path().join(LIBP2P_IDENTITY_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_or_initialize_identity(dir.path()).is_err());
    }
}
