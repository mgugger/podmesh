//! Tenant-derived DHT key for proxy provider lookup.
//!
//! The proxy announces itself in the DHT under an opaque per-tenant key derived from
//! the tenant's owner Ed25519 public key:
//!
//! ```text
//! proxy_dht_key = blake3(owner_pubkey_bytes)[..16]
//! ```
//!
//! Sidecars compute the same key from `SidecarMetadata.owner_public_key_b64` and
//! perform a Kademlia provider lookup for it. The blake3 truncation is one-way and
//! observers of the DHT cannot recover the owner pubkey from the key.

/// Compute the opaque DHT key bytes for a tenant proxy announcement.
///
/// Returns the first 16 bytes of `blake3(owner_pubkey_bytes)`.
/// The input is the **base64-decoded** raw bytes of the owner's Ed25519 public key.
pub fn compute_tenant_proxy_dht_key(owner_pubkey_b64: &str) -> anyhow::Result<Vec<u8>> {
    let pk_bytes = crypto::b64_decode(owner_pubkey_b64)?;
    Ok(compute_tenant_proxy_dht_key_from_bytes(&pk_bytes))
}

/// Same as [`compute_tenant_proxy_dht_key`] but takes raw bytes directly.
pub fn compute_tenant_proxy_dht_key_from_bytes(owner_pubkey_bytes: &[u8]) -> Vec<u8> {
    let hash = blake3::hash(owner_pubkey_bytes);
    hash.as_bytes()[..16].to_vec()
}

/// Hex-encoded form of [`compute_tenant_proxy_dht_key`] — useful for logs and CLI output.
pub fn compute_tenant_proxy_dht_key_hex(owner_pubkey_b64: &str) -> anyhow::Result<String> {
    let bytes = compute_tenant_proxy_dht_key(owner_pubkey_b64)?;
    Ok(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_same_owner() {
        // Use a base64-encoded 32-byte key (Ed25519 pubkey size).
        let pk = crypto::b64_encode(&[0x42u8; 32]);
        let k1 = compute_tenant_proxy_dht_key(&pk).unwrap();
        let k2 = compute_tenant_proxy_dht_key(&pk).unwrap();
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 16, "key must be 16 bytes");
    }

    #[test]
    fn different_owners_yield_different_keys() {
        let a = crypto::b64_encode(&[0x01u8; 32]);
        let b = crypto::b64_encode(&[0x02u8; 32]);
        let ka = compute_tenant_proxy_dht_key(&a).unwrap();
        let kb = compute_tenant_proxy_dht_key(&b).unwrap();
        assert_ne!(ka, kb);
    }

    #[test]
    fn hex_form_is_32_chars() {
        let pk = crypto::b64_encode(&[0xaau8; 32]);
        let h = compute_tenant_proxy_dht_key_hex(&pk).unwrap();
        assert_eq!(h.len(), 32, "16 bytes hex-encoded = 32 chars");
    }

    #[test]
    fn rejects_invalid_base64() {
        assert!(compute_tenant_proxy_dht_key("not!valid!base64!").is_err());
    }
}
