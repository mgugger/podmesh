//! Per-tenant credentials for a proxy's workload relay.
//!
//! The relay is a shared transport, so admission to it used to be a single
//! secret handed to every sidecar in the mesh. One leaked pod image therefore
//! leaked relay access for every tenant, and there was no way to cut off one
//! tenant without re-issuing the credential to all of them.
//!
//! A token is now derived from the mesh secret and the tenant it belongs to, and
//! carries the tenant in the clear so the relay knows who is connecting before
//! it decides. Deriving keeps the proxies stateless — any proxy holding the mesh
//! secret validates any tenant's token without shared storage — while making a
//! leaked token useless for any tenant but its own.
//!
//! This authorises *transport*, nothing more. What a workload may register or
//! tunnel is decided by its owner-signed credential, so a leaked relay token
//! costs bandwidth, not authority.

use anyhow::{Context, Result, ensure};

/// Separates the tenant from the derived proof.
///
/// Base64 never produces this character, so the split is unambiguous.
const TENANT_SEPARATOR: char = '.';

/// Domain separator, so the mesh secret cannot be reused to derive anything
/// else that happens to hash the same inputs.
const DERIVATION_CONTEXT: &str = "podmesh/workload-relay-token/v1";

/// Identity a proxy uses for its own relay connections.
///
/// A proxy is mesh infrastructure rather than a tenant, but it still dials its
/// own relay and so still needs a token. The name contains a character standard
/// base64 never emits, so it cannot collide with a namespace owner key.
pub const MESH_COMPONENT_TENANT: &str = "podmesh-mesh-component";

/// Shortest mesh secret accepted. The derived token inherits its strength.
pub const MIN_RELAY_MESH_SECRET_BYTES: usize = 32;
/// Longest mesh secret accepted, bounding what a bootstrap response may carry.
pub const MAX_RELAY_MESH_SECRET_BYTES: usize = 4 * 1024;

/// Derive the relay token for one tenant.
///
/// `owner_pubkey_b64` is the namespace owner's base64 Ed25519 public key, which
/// is public: the secrecy is entirely in `mesh_secret`.
pub fn derive_tenant_relay_token(mesh_secret: &str, owner_pubkey_b64: &str) -> Result<String> {
    validate_mesh_secret(mesh_secret)?;
    ensure!(
        !owner_pubkey_b64.is_empty() && !owner_pubkey_b64.contains(TENANT_SEPARATOR),
        "tenant owner key is not usable in a relay token"
    );
    let mut hasher = blake3::Hasher::new_derive_key(DERIVATION_CONTEXT);
    hasher.update(mesh_secret.as_bytes());
    hasher.update(&[0]);
    hasher.update(owner_pubkey_b64.as_bytes());
    let proof = crypto::b64_encode(hasher.finalize().as_bytes());
    Ok(format!("{owner_pubkey_b64}{TENANT_SEPARATOR}{proof}"))
}

/// The tenant a presented token belongs to, or an error if it is not genuine.
///
/// The comparison is over blake3 hashes, which compare in constant time, so a
/// caller cannot learn a valid token byte by byte from timing.
pub fn tenant_from_relay_token(mesh_secret: &str, token: &str) -> Result<String> {
    let (owner, _) = token
        .split_once(TENANT_SEPARATOR)
        .context("relay token does not name a tenant")?;
    let expected = derive_tenant_relay_token(mesh_secret, owner)?;
    ensure!(
        blake3::hash(expected.as_bytes()) == blake3::hash(token.as_bytes()),
        "relay token is not valid for the tenant it names"
    );
    Ok(owner.to_string())
}

pub fn validate_mesh_secret(mesh_secret: &str) -> Result<()> {
    ensure!(
        mesh_secret.len() >= MIN_RELAY_MESH_SECRET_BYTES
            && mesh_secret.len() <= MAX_RELAY_MESH_SECRET_BYTES,
        "workload relay mesh secret length is invalid"
    );
    ensure!(
        mesh_secret.is_ascii()
            && !mesh_secret
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()),
        "workload relay mesh secret contains invalid characters"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";
    const OTHER_SECRET: &str = "fedcba9876543210fedcba9876543210";

    fn owner(seed: u8) -> String {
        crypto::b64_encode(&[seed; 32])
    }

    #[test]
    fn a_derived_token_identifies_its_own_tenant() {
        let token = derive_tenant_relay_token(SECRET, &owner(1)).unwrap();
        assert_eq!(tenant_from_relay_token(SECRET, &token).unwrap(), owner(1));
    }

    /// The point of scoping: one tenant's token must not admit another, so a
    /// leaked pod image cannot be used to relay for the rest of the mesh.
    #[test]
    fn one_tenants_token_does_not_authorize_another() {
        let token = derive_tenant_relay_token(SECRET, &owner(1)).unwrap();
        let forged = token.replace(&owner(1), &owner(2));
        assert!(tenant_from_relay_token(SECRET, &forged).is_err());
    }

    #[test]
    fn tokens_differ_between_tenants_and_between_meshes() {
        let first = derive_tenant_relay_token(SECRET, &owner(1)).unwrap();
        let second = derive_tenant_relay_token(SECRET, &owner(2)).unwrap();
        let elsewhere = derive_tenant_relay_token(OTHER_SECRET, &owner(1)).unwrap();
        assert_ne!(first, second);
        assert_ne!(first, elsewhere);
    }

    #[test]
    fn a_token_from_another_mesh_is_refused() {
        let token = derive_tenant_relay_token(OTHER_SECRET, &owner(1)).unwrap();
        assert!(tenant_from_relay_token(SECRET, &token).is_err());
    }

    #[test]
    fn a_malformed_token_is_refused() {
        assert!(tenant_from_relay_token(SECRET, "no-separator").is_err());
        assert!(tenant_from_relay_token(SECRET, ".").is_err());
        assert!(tenant_from_relay_token(SECRET, "").is_err());
    }

    #[test]
    fn a_weak_mesh_secret_is_refused() {
        assert!(derive_tenant_relay_token("short", &owner(1)).is_err());
        assert!(derive_tenant_relay_token(&"a".repeat(8192), &owner(1)).is_err());
        assert!(derive_tenant_relay_token(&format!("{SECRET} "), &owner(1)).is_err());
    }
}
