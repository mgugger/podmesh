//! Per-peer nonce replay cache for envelope validation.
//!
//! Every accepted nonce is remembered for the duration of the drift window, so
//! a captured envelope cannot be presented twice. The cache is bounded on both
//! axes and evicts in FIFO order, because a cache that fails closed when full
//! would let one peer deny service to everybody else.

use anyhow::{Result, ensure};
use log::warn;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Maximum number of distinct peers tracked at once.
const MAX_TRACKED_PEERS: usize = 10_000;
/// Maximum nonces remembered per peer.
const MAX_NONCES_PER_PEER: usize = 1_000;
/// Longest nonce string accepted. Nonces are UUIDs in practice; the bound stops
/// a peer from filling its bucket with megabyte-sized keys.
pub const MAX_NONCE_LEN: usize = 128;

/// One peer's nonces, with insertion order kept so eviction is O(1).
#[derive(Default)]
struct PeerNonces {
    seen: HashMap<String, Instant>,
    order: VecDeque<String>,
}

impl PeerNonces {
    fn prune_expired(&mut self, now: Instant, window: Duration) {
        while let Some(oldest) = self.order.front() {
            match self.seen.get(oldest) {
                Some(recorded) if now.duration_since(*recorded) > window => {
                    let key = self.order.pop_front().expect("front checked above");
                    self.seen.remove(&key);
                }
                Some(_) => break,
                None => {
                    self.order.pop_front();
                }
            }
        }
    }

    fn insert(&mut self, nonce: &str, now: Instant) {
        if self.order.len() >= MAX_NONCES_PER_PEER
            && let Some(evicted) = self.order.pop_front()
        {
            self.seen.remove(&evicted);
        }
        self.seen.insert(nonce.to_string(), now);
        self.order.push_back(nonce.to_string());
    }
}

/// The whole cache, with peer insertion order kept for the same reason.
#[derive(Default)]
struct NonceStore {
    peers: HashMap<String, PeerNonces>,
    order: VecDeque<String>,
}

static NONCE_STORE: OnceLock<Mutex<NonceStore>> = OnceLock::new();

fn nonce_store() -> &'static Mutex<NonceStore> {
    NONCE_STORE.get_or_init(|| Mutex::new(NonceStore::default()))
}

/// Record `nonce` against `peer_id`, refusing it if already seen in `window`.
pub fn check_and_insert_nonce_for_peer(nonce: &str, window: Duration, peer_id: &str) -> Result<()> {
    ensure!(!nonce.is_empty(), "nonce cannot be empty");
    ensure!(
        nonce.len() <= MAX_NONCE_LEN,
        "nonce is {} bytes, over the {MAX_NONCE_LEN} byte limit",
        nonce.len()
    );

    let now = Instant::now();
    let mut store = nonce_store()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if !store.peers.contains_key(peer_id)
        && store.order.len() >= MAX_TRACKED_PEERS
        && let Some(evicted) = store.order.pop_front()
    {
        warn!("nonce store at {MAX_TRACKED_PEERS} peers, evicting oldest peer {evicted}");
        store.peers.remove(&evicted);
    }

    if !store.peers.contains_key(peer_id) {
        store
            .peers
            .insert(peer_id.to_string(), PeerNonces::default());
        store.order.push_back(peer_id.to_string());
    }

    let peer = store
        .peers
        .get_mut(peer_id)
        .expect("peer inserted immediately above");
    peer.prune_expired(now, window);
    ensure!(
        !peer.seen.contains_key(nonce),
        "replay detected: nonce already seen for this peer"
    );
    peer.insert(nonce, now);
    Ok(())
}

/// Clear the in-memory nonce store. Intended for tests only — exposed without
/// `#[cfg(test)]` so that downstream crates' integration tests can call it
/// (a `#[cfg(test)]` item in a library is not visible to tests in dependent
/// crates).
pub fn reset_nonce_store_for_test() {
    let mut store = nonce_store()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    store.peers.clear();
    store.order.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);

    #[test]
    fn replayed_nonce_is_refused_for_the_same_peer() {
        assert!(check_and_insert_nonce_for_peer("nonce-a", WINDOW, "peer-1").is_ok());
        assert!(check_and_insert_nonce_for_peer("nonce-a", WINDOW, "peer-1").is_err());
    }

    #[test]
    fn the_same_nonce_from_a_different_peer_is_accepted() {
        assert!(check_and_insert_nonce_for_peer("nonce-b", WINDOW, "peer-2").is_ok());
        assert!(check_and_insert_nonce_for_peer("nonce-b", WINDOW, "peer-3").is_ok());
    }

    #[test]
    fn oversized_and_empty_nonces_are_refused() {
        assert!(check_and_insert_nonce_for_peer("", WINDOW, "peer-4").is_err());
        let oversized = "n".repeat(MAX_NONCE_LEN + 1);
        assert!(check_and_insert_nonce_for_peer(&oversized, WINDOW, "peer-4").is_err());
    }

    #[test]
    fn a_single_peer_cannot_exhaust_the_store() {
        // Filling one peer's bucket past its bound must evict that peer's own
        // oldest entries rather than start failing closed.
        for index in 0..(MAX_NONCES_PER_PEER + 16) {
            check_and_insert_nonce_for_peer(&format!("bulk-{index}"), WINDOW, "peer-bulk")
                .expect("insert must keep succeeding under pressure");
        }
        check_and_insert_nonce_for_peer("bulk-fresh", WINDOW, "peer-bulk")
            .expect("store must still accept new nonces");
    }
}
