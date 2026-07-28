use std::path::PathBuf;

use anyhow::{Context, Result};
use iroh::SecretKey;

#[derive(Clone, Debug)]
pub enum IdentitySource {
    Persistent(PathBuf),
    Ephemeral,
}

impl IdentitySource {
    pub fn ephemeral() -> Self {
        Self::Ephemeral
    }

    pub fn load(&self) -> Result<SecretKey> {
        match self {
            Self::Persistent(key_dir) => {
                crypto::set_keypair_config(crypto::KeypairConfig {
                    signing_mode: crypto::KeypairMode::Persistent,
                    kem_mode: crypto::KeypairMode::Persistent,
                    key_directory: Some(key_dir.join("application")),
                });
                crypto::ensure_keypair_on_disk().context("load sidecar application signing key")?;
                crypto::ensure_kem_keypair_on_disk().context("load sidecar application KEM key")?;
                iroh_support::load_or_initialize_iroh_secret(&key_dir.join("iroh"))
            }
            Self::Ephemeral => Ok(SecretKey::generate()),
        }
    }
}
