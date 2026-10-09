//! The complete identity a podmesh component runs under.
//!
//! A component needs three keypairs: an Iroh transport secret, an Ed25519
//! application signing key, and an X25519 KEM key. Bundling them means every
//! component loads its identity from one explicit directory, so two components
//! in one process — or in one integration test — never share a key by accident.

use std::path::Path;

use anyhow::{Context, Result};
use iroh::SecretKey;

/// Subdirectory holding the Iroh transport secret.
pub const TRANSPORT_KEY_SUBDIR: &str = "iroh";
/// Subdirectory holding the application signing and KEM keys.
pub const APPLICATION_KEY_SUBDIR: &str = "application";

#[derive(Clone)]
pub struct NodeIdentity {
    transport_secret: SecretKey,
    signing_public: Vec<u8>,
    signing_private: Vec<u8>,
    kem_public: Vec<u8>,
    kem_private: Vec<u8>,
}

impl NodeIdentity {
    /// Load, creating on first use, every key this component needs from
    /// `key_dir`.
    pub fn load(key_dir: &Path) -> Result<Self> {
        let transport_secret =
            crate::load_or_initialize_iroh_secret(&key_dir.join(TRANSPORT_KEY_SUBDIR))?;
        let application = key_dir.join(APPLICATION_KEY_SUBDIR);
        let (signing_public, signing_private) =
            crypto::load_or_create_signing_keypair(&application)
                .context("load application signing key")?;
        let (kem_public, kem_private) =
            crypto::load_or_create_kem_keypair(&application).context("load application KEM key")?;
        Ok(Self {
            transport_secret,
            signing_public,
            signing_private,
            kem_public,
            kem_private,
        })
    }

    /// An identity that exists only for this process. Every call produces a
    /// distinct identity, so callers cannot accidentally share one.
    pub fn ephemeral() -> Self {
        let (signing_public, signing_private) = crypto::generate_signing_keypair();
        let (kem_public, kem_private) = crypto::generate_kem_keypair();
        Self {
            transport_secret: SecretKey::generate(),
            signing_public,
            signing_private,
            kem_public,
            kem_private,
        }
    }

    pub fn transport_secret(&self) -> &SecretKey {
        &self.transport_secret
    }

    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.transport_secret.public()
    }

    pub fn signing_public(&self) -> &[u8] {
        &self.signing_public
    }

    pub fn signing_private(&self) -> &[u8] {
        &self.signing_private
    }

    pub fn kem_public(&self) -> &[u8] {
        &self.kem_public
    }

    pub fn kem_private(&self) -> &[u8] {
        &self.kem_private
    }
}

impl std::fmt::Debug for NodeIdentity {
    /// Never renders private key material, so a `{:?}` of a config cannot leak
    /// an identity into a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("endpoint_id", &self.endpoint_id().fmt_short().to_string())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_is_stable_across_loads_from_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let first = NodeIdentity::load(dir.path()).unwrap();
        let second = NodeIdentity::load(dir.path()).unwrap();
        assert_eq!(first.endpoint_id(), second.endpoint_id());
        assert_eq!(first.signing_public(), second.signing_public());
        assert_eq!(first.kem_public(), second.kem_public());
    }

    #[test]
    fn distinct_directories_yield_distinct_identities() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let left = NodeIdentity::load(first.path()).unwrap();
        let right = NodeIdentity::load(second.path()).unwrap();
        assert_ne!(left.endpoint_id(), right.endpoint_id());
        assert_ne!(left.signing_public(), right.signing_public());
    }

    #[test]
    fn ephemeral_identities_are_never_shared() {
        let left = NodeIdentity::ephemeral();
        let right = NodeIdentity::ephemeral();
        assert_ne!(left.signing_public(), right.signing_public());
        assert_ne!(left.kem_public(), right.kem_public());
    }

    #[test]
    fn debug_output_does_not_contain_key_material() {
        let identity = NodeIdentity::ephemeral();
        let rendered = format!("{identity:?}");
        assert!(!rendered.contains(&crypto::b64_encode(identity.signing_private())));
        assert!(!rendered.contains(&crypto::b64_encode(identity.kem_private())));
    }
}
