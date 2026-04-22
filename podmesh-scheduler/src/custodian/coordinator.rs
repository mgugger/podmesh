//! Coordinator election via rendezvous hashing (highest-random-weight / HRW).
//!
//! Given a set of live custodian peer IDs and a manifest ID, the coordinator
//! is deterministically the peer with the highest `blake3(manifest_id || peer_id)` score.
//!
//! Properties:
//! - Deterministic: all custodians independently elect the same coordinator.
//! - Stable: adding/removing a peer only redistributes manifests assigned to that peer.
//! - No external coordination required.
//!
//! # Usage
//!
//! ```rust
//! use podmesh_scheduler::custodian::coordinator::elect_coordinator;
//!
//! let peers = vec!["peer-A".to_string(), "peer-B".to_string(), "peer-C".to_string()];
//! let coordinator = elect_coordinator("my-manifest-id", &peers).unwrap();
//! // Every node in the cluster with the same inputs will agree on the same coordinator.
//! ```

use blake3;

/// Elect the coordinator for `manifest_id` from the given slice of live custodian peer IDs.
///
/// Returns `None` if `peers` is empty.
/// Returns the peer ID string of the elected coordinator.
pub fn elect_coordinator<'a>(manifest_id: &str, peers: &'a [String]) -> Option<&'a str> {
    peers
        .iter()
        .max_by_key(|peer_id| hrw_score(manifest_id, peer_id))
        .map(|s| s.as_str())
}

/// Compute the HRW score for a (manifest_id, peer_id) pair.
///
/// Returns the first 8 bytes of blake3(manifest_id || "\x00" || peer_id) as a u64.
/// Using the full 256-bit hash as the ordering key would be more collision-resistant,
/// but u64 is sufficient for practical cluster sizes (probability of tie ≈ 1/2^64).
pub fn hrw_score(manifest_id: &str, peer_id: &str) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(manifest_id.as_bytes());
    hasher.update(b"\x00");
    hasher.update(peer_id.as_bytes());
    let hash = hasher.finalize();
    let bytes = hash.as_bytes();
    u64::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
}

/// Returns `true` if `local_peer_id` is the elected coordinator for `manifest_id`
/// given `live_peers`.
pub fn is_coordinator(manifest_id: &str, local_peer_id: &str, live_peers: &[String]) -> bool {
    elect_coordinator(manifest_id, live_peers)
        .map(|elected| elected == local_peer_id)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_peers_returns_none() {
        assert!(elect_coordinator("manifest-1", &[]).is_none());
    }

    #[test]
    fn test_single_peer_is_always_coordinator() {
        let peers = vec!["only-peer".to_string()];
        assert_eq!(elect_coordinator("any-manifest", &peers), Some("only-peer"));
    }

    #[test]
    fn test_deterministic_across_calls() {
        let peers = vec![
            "peer-A".to_string(),
            "peer-B".to_string(),
            "peer-C".to_string(),
        ];
        let a = elect_coordinator("manifest-xyz", &peers);
        let b = elect_coordinator("manifest-xyz", &peers);
        assert_eq!(a, b);
    }

    #[test]
    fn test_different_manifests_may_elect_different_coordinators() {
        let peers = vec![
            "peer-A".to_string(),
            "peer-B".to_string(),
            "peer-C".to_string(),
            "peer-D".to_string(),
            "peer-E".to_string(),
        ];
        // Run many manifests and collect elected coordinators; expect variety.
        let elected: std::collections::HashSet<String> = (0..50)
            .filter_map(|i| {
                let mid = format!("manifest-{}", i);
                elect_coordinator(&mid, &peers).map(String::from)
            })
            .collect();
        // With 5 peers and 50 manifests the probability all end up on one peer is negligible.
        assert!(elected.len() > 1, "expected load spread across peers, got {:?}", elected);
    }

    #[test]
    fn test_order_independence() {
        let peers_ab = vec!["peer-A".to_string(), "peer-B".to_string()];
        let peers_ba = vec!["peer-B".to_string(), "peer-A".to_string()];
        assert_eq!(
            elect_coordinator("m", &peers_ab),
            elect_coordinator("m", &peers_ba)
        );
    }

    #[test]
    fn test_is_coordinator() {
        let peers = vec!["peer-A".to_string(), "peer-B".to_string()];
        let coordinator = elect_coordinator("m1", &peers).unwrap().to_string();
        let other = if coordinator == "peer-A" { "peer-B" } else { "peer-A" };
        assert!(is_coordinator("m1", &coordinator, &peers));
        assert!(!is_coordinator("m1", other, &peers));
    }

    #[test]
    fn test_adding_peer_only_changes_some_elections() {
        let peers3 = vec![
            "peer-A".to_string(),
            "peer-B".to_string(),
            "peer-C".to_string(),
        ];
        let peers4 = {
            let mut p = peers3.clone();
            p.push("peer-D".to_string());
            p
        };

        let manifests: Vec<String> = (0..20).map(|i| format!("manifest-{}", i)).collect();
        let mut changed = 0usize;
        for mid in &manifests {
            let e3 = elect_coordinator(mid, &peers3).unwrap();
            let e4 = elect_coordinator(mid, &peers4).unwrap();
            if e3 != e4 {
                changed += 1;
            }
        }
        // Adding 1 peer to a 3-peer cluster should reassign ~25% of manifests (1/4).
        // Allow generous range [5%, 60%] to avoid flakiness.
        let pct = changed * 100 / manifests.len();
        assert!(
            pct >= 5 && pct <= 60,
            "expected ~25% redistribution, got {}%",
            pct
        );
    }
}
