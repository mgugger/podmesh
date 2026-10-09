//! Bounded process-owned replay protection for workload envelopes.

use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use parking_lot::Mutex;

pub const DEFAULT_REPLAY_MAX_PEERS: usize = 10_000;
pub const DEFAULT_REPLAY_NONCES_PER_PEER: usize = 1_000;
pub const DEFAULT_REPLAY_MAX_NONCE_BYTES: usize = 128;
pub const DEFAULT_REPLAY_RETENTION: Duration = Duration::from_secs(120);
pub const MAX_REPLAY_PEERS: usize = 10_000;
pub const MAX_REPLAY_NONCES_PER_PEER: usize = 1_000;
pub const MIN_REPLAY_NONCE_BYTES: usize = 36;
pub const MAX_REPLAY_NONCE_BYTES: usize = 128;
pub const MIN_REPLAY_RETENTION: Duration = Duration::from_secs(60);
pub const MAX_REPLAY_RETENTION: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayLimits {
    pub max_peers: usize,
    pub nonces_per_peer: usize,
    pub max_nonce_bytes: usize,
    pub retention: Duration,
}

impl Default for ReplayLimits {
    fn default() -> Self {
        Self {
            max_peers: DEFAULT_REPLAY_MAX_PEERS,
            nonces_per_peer: DEFAULT_REPLAY_NONCES_PER_PEER,
            max_nonce_bytes: DEFAULT_REPLAY_MAX_NONCE_BYTES,
            retention: DEFAULT_REPLAY_RETENTION,
        }
    }
}

impl ReplayLimits {
    pub fn validate(self) -> Result<Self> {
        ensure!(
            (1..=MAX_REPLAY_PEERS).contains(&self.max_peers),
            "replay peer limit is outside its allowed range"
        );
        ensure!(
            (1..=MAX_REPLAY_NONCES_PER_PEER).contains(&self.nonces_per_peer),
            "replay per-peer nonce limit is outside its allowed range"
        );
        ensure!(
            (MIN_REPLAY_NONCE_BYTES..=MAX_REPLAY_NONCE_BYTES).contains(&self.max_nonce_bytes),
            "replay nonce byte limit is outside its allowed range"
        );
        ensure!(
            (MIN_REPLAY_RETENTION..=MAX_REPLAY_RETENTION).contains(&self.retention),
            "replay retention is outside its allowed range"
        );
        Ok(self)
    }
}

#[derive(Default)]
struct PeerNonces {
    seen: HashMap<String, Instant>,
    order: VecDeque<String>,
}

impl PeerNonces {
    fn prune_expired(&mut self, now: Instant, retention: Duration) {
        while let Some(nonce) = self.order.front() {
            match self.seen.get(nonce) {
                Some(recorded) if now.saturating_duration_since(*recorded) >= retention => {
                    let nonce = self.order.pop_front().expect("front was checked");
                    self.seen.remove(&nonce);
                }
                Some(_) => break,
                None => {
                    self.order.pop_front();
                }
            }
        }
    }

    fn insert(&mut self, nonce: &str, now: Instant, limit: usize) {
        if self.order.len() >= limit
            && let Some(oldest) = self.order.pop_front()
        {
            self.seen.remove(&oldest);
        }
        self.seen.insert(nonce.to_string(), now);
        self.order.push_back(nonce.to_string());
    }
}

#[derive(Default)]
struct ReplayState {
    peers: HashMap<String, PeerNonces>,
    peer_order: VecDeque<String>,
}

pub struct PeerReplayRegistry {
    limits: ReplayLimits,
    state: Mutex<ReplayState>,
}

impl PeerReplayRegistry {
    pub fn new(limits: ReplayLimits) -> Result<Self> {
        Ok(Self {
            limits: limits.validate()?,
            state: Mutex::new(ReplayState::default()),
        })
    }

    pub fn limits(&self) -> ReplayLimits {
        self.limits
    }

    pub fn check_and_insert(&self, peer_id: &str, nonce: &str, now: Instant) -> Result<()> {
        ensure!(!peer_id.is_empty(), "replay peer id is empty");
        ensure!(!nonce.is_empty(), "envelope nonce is empty");
        ensure!(
            nonce.len() <= self.limits.max_nonce_bytes,
            "envelope nonce exceeds configured byte limit"
        );

        let mut state = self.state.lock();
        if let Some(peer) = state.peers.get_mut(peer_id) {
            peer.prune_expired(now, self.limits.retention);
            ensure!(!peer.seen.contains_key(nonce), "envelope replay detected");
            peer.insert(nonce, now, self.limits.nonces_per_peer);
            return Ok(());
        }

        if state.peers.len() >= self.limits.max_peers {
            self.prune_expired_peers(&mut state, now);
        }
        if state.peers.len() >= self.limits.max_peers
            && let Some(oldest) = state.peer_order.pop_front()
        {
            state.peers.remove(&oldest);
        }

        let mut peer = PeerNonces::default();
        peer.insert(nonce, now, self.limits.nonces_per_peer);
        state.peers.insert(peer_id.to_string(), peer);
        state.peer_order.push_back(peer_id.to_string());
        Ok(())
    }

    fn prune_expired_peers(&self, state: &mut ReplayState, now: Instant) {
        state.peers.retain(|_, peer| {
            peer.prune_expired(now, self.limits.retention);
            !peer.seen.is_empty()
        });
        state
            .peer_order
            .retain(|peer_id| state.peers.contains_key(peer_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE_A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const NONCE_B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    const NONCE_C: &str = "cccccccc-cccc-cccc-cccc-cccccccccccc";

    fn limits(max_peers: usize, nonces_per_peer: usize) -> ReplayLimits {
        ReplayLimits {
            max_peers,
            nonces_per_peer,
            ..ReplayLimits::default()
        }
    }

    #[test]
    fn limits_validate_boundaries() {
        assert!(ReplayLimits::default().validate().is_ok());
        for invalid in [
            ReplayLimits {
                max_peers: 0,
                ..ReplayLimits::default()
            },
            ReplayLimits {
                nonces_per_peer: 0,
                ..ReplayLimits::default()
            },
            ReplayLimits {
                max_nonce_bytes: MIN_REPLAY_NONCE_BYTES - 1,
                ..ReplayLimits::default()
            },
            ReplayLimits {
                retention: MIN_REPLAY_RETENTION - Duration::from_secs(1),
                ..ReplayLimits::default()
            },
            ReplayLimits {
                max_peers: MAX_REPLAY_PEERS + 1,
                ..ReplayLimits::default()
            },
            ReplayLimits {
                nonces_per_peer: MAX_REPLAY_NONCES_PER_PEER + 1,
                ..ReplayLimits::default()
            },
            ReplayLimits {
                max_nonce_bytes: MAX_REPLAY_NONCE_BYTES + 1,
                ..ReplayLimits::default()
            },
            ReplayLimits {
                retention: MAX_REPLAY_RETENTION + Duration::from_secs(1),
                ..ReplayLimits::default()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn duplicate_is_refused_for_one_peer_only() {
        let registry = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        let now = Instant::now();
        registry.check_and_insert("peer-a", NONCE_A, now).unwrap();
        assert!(registry.check_and_insert("peer-a", NONCE_A, now).is_err());
        assert!(registry.check_and_insert("peer-b", NONCE_A, now).is_ok());
    }

    #[test]
    fn reconnect_does_not_reset_peer_history() {
        let registry = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        let now = Instant::now();
        registry
            .check_and_insert("same-endpoint", NONCE_A, now)
            .unwrap();
        assert!(
            registry
                .check_and_insert("same-endpoint", NONCE_A, now + Duration::from_secs(1))
                .is_err()
        );
    }

    #[test]
    fn nonce_is_accepted_again_at_retention_boundary() {
        let registry = PeerReplayRegistry::new(ReplayLimits::default()).unwrap();
        let now = Instant::now();
        registry.check_and_insert("peer", NONCE_A, now).unwrap();
        assert!(
            registry
                .check_and_insert(
                    "peer",
                    NONCE_A,
                    now + DEFAULT_REPLAY_RETENTION - Duration::from_millis(1),
                )
                .is_err()
        );
        assert!(
            registry
                .check_and_insert("peer", NONCE_A, now + DEFAULT_REPLAY_RETENTION)
                .is_ok()
        );
    }

    #[test]
    fn full_peer_bucket_evicts_its_oldest_nonce() {
        let registry = PeerReplayRegistry::new(limits(4, 2)).unwrap();
        let now = Instant::now();
        registry.check_and_insert("peer", NONCE_A, now).unwrap();
        registry.check_and_insert("peer", NONCE_B, now).unwrap();
        registry.check_and_insert("peer", NONCE_C, now).unwrap();
        assert!(registry.check_and_insert("peer", NONCE_A, now).is_ok());
        assert!(registry.check_and_insert("peer", NONCE_C, now).is_err());
    }

    #[test]
    fn full_registry_evicts_oldest_peer_and_keeps_serving() {
        let registry = PeerReplayRegistry::new(limits(2, 2)).unwrap();
        let now = Instant::now();
        registry.check_and_insert("peer-a", NONCE_A, now).unwrap();
        registry.check_and_insert("peer-b", NONCE_A, now).unwrap();
        registry.check_and_insert("peer-c", NONCE_A, now).unwrap();
        assert!(registry.check_and_insert("peer-a", NONCE_A, now).is_ok());
        assert!(registry.check_and_insert("peer-c", NONCE_A, now).is_err());
    }

    #[test]
    fn distinct_peer_insertions_commute_below_capacity() {
        let first = PeerReplayRegistry::new(limits(4, 2)).unwrap();
        let second = PeerReplayRegistry::new(limits(4, 2)).unwrap();
        let now = Instant::now();
        first.check_and_insert("peer-a", NONCE_A, now).unwrap();
        first.check_and_insert("peer-b", NONCE_B, now).unwrap();
        second.check_and_insert("peer-b", NONCE_B, now).unwrap();
        second.check_and_insert("peer-a", NONCE_A, now).unwrap();
        for registry in [&first, &second] {
            assert!(registry.check_and_insert("peer-a", NONCE_A, now).is_err());
            assert!(registry.check_and_insert("peer-b", NONCE_B, now).is_err());
        }
    }
}
