//! Keypair manager to centralize crypto keypair operations
//!
//! This module provides a unified interface for managing cryptographic keypairs
//! and eliminates duplication of keypair generation and handling patterns.

use crate::logging::CryptoLogger;
use anyhow::Result;
use ed25519_dalek::SigningKey;
use log::{info, warn};
use once_cell::sync::OnceCell;
use std::sync::Mutex;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

/// Keypair operation error types
#[derive(Debug, thiserror::Error)]
pub enum KeypairError {
    #[error("Keypair generation failed: {0}")]
    GenerationFailed(String),
    #[error("Keypair loading failed: {0}")]
    LoadingFailed(String),
    #[error("Keypair storage failed: {0}")]
    StorageFailed(String),
    #[error("Invalid keypair format: {0}")]
    InvalidFormat(String),
}

/// Keypair operation result type
pub type KeypairResult<T> = Result<T, KeypairError>;

/// Cached ephemeral keypairs for testing and development
static EPHEMERAL_SIGNING_KEYPAIR: OnceCell<Mutex<Option<(Vec<u8>, Vec<u8>)>>> = OnceCell::new();
static EPHEMERAL_KEM_KEYPAIR: OnceCell<Mutex<Option<(Vec<u8>, Vec<u8>)>>> = OnceCell::new();

/// Keypair types supported by the system
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeypairType {
    Signing,
    Kem,
}

impl std::fmt::Display for KeypairType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeypairType::Signing => write!(f, "signing"),
            KeypairType::Kem => write!(f, "kem"),
        }
    }
}

/// Keypair storage mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    Persistent,
    Ephemeral,
}

/// Centralized keypair manager
pub struct KeypairManager;

impl KeypairManager {
    /// Generate or retrieve signing keypair
    pub fn get_signing_keypair(mode: StorageMode) -> KeypairResult<(Vec<u8>, Vec<u8>)> {
        match mode {
            StorageMode::Persistent => Self::get_persistent_signing_keypair(),
            StorageMode::Ephemeral => Self::get_ephemeral_signing_keypair(),
        }
    }

    /// Generate or retrieve KEM keypair
    pub fn get_kem_keypair(mode: StorageMode) -> KeypairResult<(Vec<u8>, Vec<u8>)> {
        match mode {
            StorageMode::Persistent => Self::get_persistent_kem_keypair(),
            StorageMode::Ephemeral => Self::get_ephemeral_kem_keypair(),
        }
    }

    /// Get persistent signing keypair from disk
    pub fn get_persistent_signing_keypair() -> KeypairResult<(Vec<u8>, Vec<u8>)> {
        crate::ensure_keypair_on_disk()
            .map_err(|e| KeypairError::LoadingFailed(format!("signing keypair: {}", e)))
            .and_then(|(pub_bytes, priv_bytes)| {
                CryptoLogger::log_crypto_operation(
                    "load_signing_keypair",
                    true,
                    Some("persistent storage"),
                );
                info!(
                    "keypair: loaded persistent signing keypair, pub_len={}, priv_len={}",
                    pub_bytes.len(),
                    priv_bytes.len()
                );
                Ok((pub_bytes, priv_bytes))
            })
    }

    /// Get persistent KEM keypair from disk
    pub fn get_persistent_kem_keypair() -> KeypairResult<(Vec<u8>, Vec<u8>)> {
        crate::ensure_kem_keypair_on_disk()
            .map_err(|e| KeypairError::LoadingFailed(format!("KEM keypair: {}", e)))
            .and_then(|(pub_bytes, priv_bytes)| {
                CryptoLogger::log_crypto_operation(
                    "load_kem_keypair",
                    true,
                    Some("persistent storage"),
                );
                info!(
                    "keypair: loaded persistent KEM keypair, pub_len={}, priv_len={}",
                    pub_bytes.len(),
                    priv_bytes.len()
                );
                Ok((pub_bytes, priv_bytes))
            })
    }

    /// Get or generate ephemeral signing keypair (cached for process lifetime)
    pub fn get_ephemeral_signing_keypair() -> KeypairResult<(Vec<u8>, Vec<u8>)> {
        let cache = EPHEMERAL_SIGNING_KEYPAIR.get_or_init(|| Mutex::new(None));
        let mut guard = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(ref keypair) = *guard {
            return Ok(keypair.clone());
        }

        // Generate new ephemeral keypair
        let (pub_bytes, priv_bytes) = crate::ensure_keypair_ephemeral().map_err(|e| {
            KeypairError::GenerationFailed(format!("ephemeral signing keypair: {}", e))
        })?;

        CryptoLogger::log_crypto_operation(
            "generate_ephemeral_signing_keypair",
            true,
            Some("in-memory cache"),
        );
        info!(
            "keypair: generated ephemeral signing keypair, pub_len={}, priv_len={}",
            pub_bytes.len(),
            priv_bytes.len()
        );

        *guard = Some((pub_bytes.clone(), priv_bytes.clone()));
        Ok((pub_bytes, priv_bytes))
    }

    /// Get or generate ephemeral KEM keypair (cached for process lifetime)
    pub fn get_ephemeral_kem_keypair() -> KeypairResult<(Vec<u8>, Vec<u8>)> {
        let cache = EPHEMERAL_KEM_KEYPAIR.get_or_init(|| Mutex::new(None));
        let mut guard = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(ref keypair) = *guard {
            return Ok(keypair.clone());
        }

        // Generate new ephemeral X25519 keypair
        let mut rng = rand::rngs::OsRng;
        let secret = StaticSecret::random_from_rng(&mut rng);
        let public = X25519PublicKey::from(&secret);

        let pub_bytes = public.as_bytes().to_vec();
        let priv_bytes = secret.as_bytes().to_vec();

        CryptoLogger::log_crypto_operation(
            "generate_ephemeral_kem_keypair",
            true,
            Some("in-memory cache"),
        );
        info!(
            "keypair: generated ephemeral KEM keypair, pub_len={}, priv_len={}",
            pub_bytes.len(),
            priv_bytes.len()
        );

        *guard = Some((pub_bytes.clone(), priv_bytes.clone()));
        Ok((pub_bytes, priv_bytes))
    }

    /// Generate fresh keypair without caching
    pub fn generate_fresh_keypair(keypair_type: KeypairType) -> KeypairResult<(Vec<u8>, Vec<u8>)> {
        match keypair_type {
            KeypairType::Signing => {
                let mut rng = rand::rngs::OsRng;
                let signing_key = SigningKey::generate(&mut rng);
                let verifying_key = signing_key.verifying_key();

                let pub_bytes = verifying_key.to_bytes().to_vec();
                let priv_bytes = signing_key.to_bytes().to_vec();

                CryptoLogger::log_crypto_operation(
                    "generate_fresh_signing_keypair",
                    true,
                    Some("no caching"),
                );
                Ok((pub_bytes, priv_bytes))
            }
            KeypairType::Kem => {
                let mut rng = rand::rngs::OsRng;
                let secret = StaticSecret::random_from_rng(&mut rng);
                let public = X25519PublicKey::from(&secret);

                let pub_bytes = public.as_bytes().to_vec();
                let priv_bytes = secret.as_bytes().to_vec();

                CryptoLogger::log_crypto_operation(
                    "generate_fresh_kem_keypair",
                    true,
                    Some("no caching"),
                );
                Ok((pub_bytes, priv_bytes))
            }
        }
    }

    /// Sign data with signing keypair
    pub fn sign_with_keypair(data: &[u8], mode: StorageMode) -> KeypairResult<(String, String)> {
        let (pub_bytes, priv_bytes) = Self::get_signing_keypair(mode)?;

        crate::sign_envelope(&priv_bytes, &pub_bytes, data)
            .map_err(|e| KeypairError::GenerationFailed(format!("signing operation: {}", e)))
            .and_then(|(sig_b64, pub_b64)| {
                CryptoLogger::log_crypto_operation("sign_data", true, Some("envelope signing"));
                Ok((sig_b64, pub_b64))
            })
    }

    /// Verify signature with public key
    pub fn verify_signature(
        pub_bytes: &[u8],
        data: &[u8],
        sig_bytes: &[u8],
    ) -> KeypairResult<bool> {
        match crate::verify_envelope(pub_bytes, data, sig_bytes) {
            Ok(()) => {
                CryptoLogger::log_signature_verification(true, None);
                Ok(true)
            }
            Err(e) => {
                CryptoLogger::log_signature_verification(false, None);
                warn!("keypair: signature verification failed: {:?}", e);
                Ok(false)
            }
        }
    }

    /// Encapsulate to KEM public key
    pub fn encapsulate_to_pubkey(pub_bytes: &[u8]) -> KeypairResult<(Vec<u8>, Vec<u8>)> {
        crate::encapsulate_to_pubkey(pub_bytes)
            .map_err(|e| KeypairError::GenerationFailed(format!("KEM encapsulation: {}", e)))
            .and_then(|(ciphertext, shared_secret)| {
                CryptoLogger::log_crypto_operation("kem_encapsulate", true, None);
                Ok((ciphertext, shared_secret))
            })
    }

    /// Decapsulate KEM ciphertext
    pub fn decapsulate_with_keypair(
        ciphertext: &[u8],
        mode: StorageMode,
    ) -> KeypairResult<zeroize::Zeroizing<Vec<u8>>> {
        let (_pub_bytes, priv_bytes) = Self::get_kem_keypair(mode)?;

        crate::decapsulate_share(&priv_bytes, ciphertext)
            .map_err(|e| KeypairError::GenerationFailed(format!("KEM decapsulation: {}", e)))
            .and_then(|shared_secret| {
                CryptoLogger::log_crypto_operation("kem_decapsulate", true, None);
                Ok(shared_secret)
            })
    }

    /// Clear ephemeral keypair caches (useful for testing)
    pub fn clear_ephemeral_caches() {
        if let Some(cache) = EPHEMERAL_SIGNING_KEYPAIR.get() {
            let mut guard = cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = None;
        }
        if let Some(cache) = EPHEMERAL_KEM_KEYPAIR.get() {
            let mut guard = cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = None;
        }
        info!("keypair: cleared ephemeral keypair caches");
    }

    /// Get keypair information without exposing private key material
    pub fn get_keypair_info(
        keypair_type: KeypairType,
        mode: StorageMode,
    ) -> KeypairResult<KeypairInfo> {
        let (pub_bytes, priv_bytes) = match keypair_type {
            KeypairType::Signing => Self::get_signing_keypair(mode)?,
            KeypairType::Kem => Self::get_kem_keypair(mode)?,
        };

        Ok(KeypairInfo {
            keypair_type,
            storage_mode: mode,
            public_key_len: pub_bytes.len(),
            private_key_len: priv_bytes.len(),
        })
    }

    /// Determine storage mode from the global keypair configuration
    pub fn default_storage_mode() -> StorageMode {
        match crate::get_keypair_config().signing_mode {
            crate::KeypairMode::Ephemeral => StorageMode::Ephemeral,
            crate::KeypairMode::Persistent => StorageMode::Persistent,
        }
    }
}

/// Information about a keypair (without exposing key material)
#[derive(Debug, Clone)]
pub struct KeypairInfo {
    pub keypair_type: KeypairType,
    pub storage_mode: StorageMode,
    pub public_key_len: usize,
    pub private_key_len: usize,
}

/// Convenience functions for common patterns
impl KeypairManager {
    /// Get signing keypair using default storage mode
    pub fn get_default_signing_keypair() -> KeypairResult<(Vec<u8>, Vec<u8>)> {
        Self::get_signing_keypair(Self::default_storage_mode())
    }

    /// Get KEM keypair using default storage mode
    pub fn get_default_kem_keypair() -> KeypairResult<(Vec<u8>, Vec<u8>)> {
        Self::get_kem_keypair(Self::default_storage_mode())
    }

    /// Sign with default keypair
    pub fn sign_with_default_keypair(data: &[u8]) -> KeypairResult<(String, String)> {
        Self::sign_with_keypair(data, Self::default_storage_mode())
    }

    /// Decapsulate with default keypair
    pub fn decapsulate_with_default_keypair(
        ciphertext: &[u8],
    ) -> KeypairResult<zeroize::Zeroizing<Vec<u8>>> {
        Self::decapsulate_with_keypair(ciphertext, Self::default_storage_mode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_storage_mode() {
        crate::set_keypair_config(crate::KeypairConfig::default());
        assert_eq!(
            KeypairManager::default_storage_mode(),
            StorageMode::Persistent
        );

        crate::set_keypair_config(crate::KeypairConfig {
            signing_mode: crate::KeypairMode::Ephemeral,
            kem_mode: crate::KeypairMode::Ephemeral,
            key_directory: None,
        });
        assert_eq!(
            KeypairManager::default_storage_mode(),
            StorageMode::Ephemeral
        );
        crate::set_keypair_config(crate::KeypairConfig::default());
    }

    #[test]
    fn test_ephemeral_keypair_generation() {
        // Clear caches first
        KeypairManager::clear_ephemeral_caches();

        // Generate ephemeral signing keypair
        let result = KeypairManager::get_ephemeral_signing_keypair();
        assert!(result.is_ok());

        let (pub1, priv1) = result.unwrap();
        assert!(!pub1.is_empty());
        assert!(!priv1.is_empty());

        // Second call should return cached keypair (same keys)
        let (pub2, priv2) = KeypairManager::get_ephemeral_signing_keypair().unwrap();
        assert_eq!(pub1, pub2);
        assert_eq!(priv1, priv2);
    }

    #[test]
    fn test_fresh_keypair_generation() {
        // Generate fresh signing keypairs (should be different each time)
        let result1 = KeypairManager::generate_fresh_keypair(KeypairType::Signing);
        let result2 = KeypairManager::generate_fresh_keypair(KeypairType::Signing);

        assert!(result1.is_ok());
        assert!(result2.is_ok());

        let (pub1, priv1) = result1.unwrap();
        let (pub2, priv2) = result2.unwrap();

        // Fresh keypairs should be different
        assert_ne!(pub1, pub2);
        assert_ne!(priv1, priv2);
    }

    #[test]
    fn test_keypair_info() {
        let info = KeypairManager::get_keypair_info(KeypairType::Signing, StorageMode::Ephemeral);

        if info.is_ok() {
            let info = info.unwrap();
            assert_eq!(info.keypair_type, KeypairType::Signing);
            assert_eq!(info.storage_mode, StorageMode::Ephemeral);
            assert!(info.public_key_len > 0);
            assert!(info.private_key_len > 0);
        }
    }

    #[test]
    fn test_clear_ephemeral_caches() {
        // This test verifies the function runs without panicking
        KeypairManager::clear_ephemeral_caches();
        // If we get here, the function didn't panic
    }
}
