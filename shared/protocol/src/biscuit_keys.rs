//! Bridging between Podmesh's raw Ed25519 identity keys and Biscuit's key types.
//!
//! Podmesh mints exactly one kind of Biscuit today — the owner-signed proxy
//! grant in [`crate::proxy_grant`] — and the owner's namespace key doubles as
//! the Biscuit root key. These helpers are the only place that conversion
//! happens, so the mapping stays in one spot if a second token type appears.

use anyhow::{Context, Result};
use biscuit_auth::{Algorithm, KeyPair, PublicKey};

/// Largest Biscuit accepted from the wire, checked before any parsing.
pub const MAX_BISCUIT_TOKEN_BYTES: usize = 16 * 1024;
/// Longest string admitted as a Biscuit fact value.
pub const MAX_AUTHZ_VALUE_LEN: usize = 256;

pub fn biscuit_keypair_from_ed25519(private_key: &[u8]) -> Result<KeyPair> {
    KeyPair::from_bytes(private_key, Algorithm::Ed25519.into())
        .context("decode Biscuit Ed25519 private key")
}

pub fn biscuit_public_key_from_ed25519(public_key: &[u8]) -> Result<PublicKey> {
    PublicKey::from_bytes(public_key, Algorithm::Ed25519)
        .context("decode Biscuit Ed25519 public key")
}
