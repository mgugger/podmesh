use anyhow::Context;
use log::warn;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Maximum number of unique peers to track in the nonce store (prevents memory exhaustion)
const MAX_TRACKED_PEERS: usize = 10_000;

/// Maximum nonces per peer to prevent memory exhaustion from a single peer
const MAX_NONCES_PER_PEER: usize = 1_000;

static NONCE_STORE: OnceLock<Mutex<HashMap<String, HashMap<String, Instant>>>> = OnceLock::new();

fn nonce_store() -> &'static Mutex<HashMap<String, HashMap<String, Instant>>> {
    NONCE_STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Accept a signature string potentially prefixed (e.g. "ed25519:<b64>") and return decoded bytes.
/// Keeps prefix-handling logic centralized.
pub fn normalize_and_decode_signature(sig_opt: Option<&str>) -> anyhow::Result<Vec<u8>> {
    let sig_str = sig_opt.unwrap_or("");
    let b64_part = if let Some(idx) = sig_str.find(':') {
        &sig_str[idx + 1..]
    } else {
        sig_str
    };
    crate::b64_decode(b64_part)
        .context("failed to base64-decode signature")
}

/// Check replay protection: ensure nonce is not seen in `nonce_window` and insert it.
/// Returns Err if duplicate or invalid.
pub fn check_and_insert_nonce(nonce_str: &str, nonce_window: Duration) -> anyhow::Result<()> {
    check_and_insert_nonce_for_peer(nonce_str, nonce_window, "global")
}

/// Check replay protection for a specific peer: ensure nonce is not seen in `nonce_window` and insert it.
/// Returns Err if duplicate or invalid.
pub fn check_and_insert_nonce_for_peer(
    nonce_str: &str,
    nonce_window: Duration,
    peer_id: &str,
) -> anyhow::Result<()> {
    if nonce_str.is_empty() {
        return Err(anyhow::anyhow!("nonce cannot be empty"));
    }

    let now = Instant::now();
    let mut store = nonce_store()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // Enforce maximum peer count with LRU-like eviction
    if !store.contains_key(peer_id) && store.len() >= MAX_TRACKED_PEERS {
        // Evict the peer with the oldest average nonce timestamp
        if let Some(oldest_peer) = store
            .iter()
            .filter_map(|(k, inner)| {
                inner.values().min().map(|oldest_time| (k.clone(), *oldest_time))
            })
            .min_by_key(|(_, time)| *time)
            .map(|(k, _)| k)
        {
            warn!(
                "nonce store at capacity ({} peers), evicting oldest peer: {}",
                MAX_TRACKED_PEERS, oldest_peer
            );
            store.remove(&oldest_peer);
        }
    }

    // Get or create peer-specific nonce store
    let peer_store = store
        .entry(peer_id.to_string())
        .or_insert_with(HashMap::new);

    // prune old nonces for this peer
    peer_store.retain(|_, &mut t| now.duration_since(t) <= nonce_window);

    // Enforce maximum nonces per peer
    if peer_store.len() >= MAX_NONCES_PER_PEER {
        // Evict the oldest nonce for this peer
        if let Some(oldest_nonce) = peer_store
            .iter()
            .min_by_key(|(_, time)| *time)
            .map(|(k, _)| k.clone())
        {
            warn!(
                "peer {} at nonce capacity ({}), evicting oldest nonce",
                peer_id, MAX_NONCES_PER_PEER
            );
            peer_store.remove(&oldest_nonce);
        }
    }

    if peer_store.contains_key(nonce_str) {
        return Err(anyhow::anyhow!("replay detected: nonce already seen"));
    }
    peer_store.insert(nonce_str.to_string(), now);
    Ok(())
}

/// Clear the in-memory nonce store. Intended for tests only — exposed without
/// `#[cfg(test)]` so that downstream crates' integration tests can call it
/// (a `#[cfg(test)]` item in a library is not visible to tests in dependent
/// crates).
pub fn reset_nonce_store_for_test() {
    nonce_store()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_nonce_replay_protection() {
        let nonce = "test-nonce-unique";
        let window = Duration::from_secs(60);

        // First use should succeed
        assert!(check_and_insert_nonce(nonce, window).is_ok());

        // Second use should fail (replay)
        assert!(check_and_insert_nonce(nonce, window).is_err());
    }

    #[test]
    fn test_signature_prefix_handling() {
        // Test with prefix
        let sig_with_prefix = "ed25519:dGVzdA=="; // "test" in base64
        let decoded = normalize_and_decode_signature(Some(sig_with_prefix)).unwrap();
        assert_eq!(decoded, b"test");

        // Test without prefix
        let sig_without_prefix = "dGVzdA==";
        let decoded = normalize_and_decode_signature(Some(sig_without_prefix)).unwrap();
        assert_eq!(decoded, b"test");
    }
}
