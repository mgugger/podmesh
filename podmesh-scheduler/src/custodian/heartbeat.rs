//! Phase 6: Custodian heartbeat and liveness tracking.
//!
//! Each custodian node broadcasts a `HeartbeatPing` on the main gossipsub topic
//! every `HEARTBEAT_INTERVAL_SECS` seconds. Any node that receives a ping updates
//! its `CustodianLivenessTracker`.
//!
//! When a custodian entry exceeds `HEARTBEAT_EXPIRY_SECS` without a ping, it is
//! considered dead. The coordinator (elected via HRW) broadcasts a
//! `CustodianWithdraw` on the relevant topic and logs the event for future
//! re-distribution logic (Phase 6 completion).

use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use protocol::machine::HeartbeatPing;

/// Seconds between heartbeat broadcasts.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 10;
/// Seconds after which a custodian is considered dead.
pub const HEARTBEAT_EXPIRY_SECS: u64 = 35;

#[derive(Debug, Clone)]
pub struct CustodianLivenessEntry {
    pub last_seen: Instant,
    /// Manifest IDs this custodian holds shares for.
    pub manifest_ids: Vec<String>,
}

/// Global liveness map: peer_id_str → entry.
static LIVENESS: Lazy<Mutex<HashMap<String, CustodianLivenessEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Update or insert a custodian's liveness entry from a received `HeartbeatPing`.
pub fn record_heartbeat(ping: &HeartbeatPing) {
    let mut map = LIVENESS.lock().unwrap_or_else(|p| p.into_inner());
    map.insert(
        ping.peer_id.clone(),
        CustodianLivenessEntry {
            last_seen: Instant::now(),
            manifest_ids: ping.custodian_manifest_ids.clone(),
        },
    );
}

/// Return peer IDs of custodians that have not sent a heartbeat within `HEARTBEAT_EXPIRY_SECS`.
pub fn expired_custodians() -> Vec<(String, Vec<String>)> {
    let map = LIVENESS.lock().unwrap_or_else(|p| p.into_inner());
    let cutoff = Duration::from_secs(HEARTBEAT_EXPIRY_SECS);
    map.iter()
        .filter(|(_, entry)| entry.last_seen.elapsed() > cutoff)
        .map(|(peer_id, entry)| (peer_id.clone(), entry.manifest_ids.clone()))
        .collect()
}

/// Remove a custodian from the liveness map (called after emitting CustodianWithdraw).
pub fn remove_liveness_entry(peer_id: &str) {
    let mut map = LIVENESS.lock().unwrap_or_else(|p| p.into_inner());
    map.remove(peer_id);
}

/// Return a snapshot of the current liveness map for diagnostics.
pub fn liveness_snapshot() -> Vec<(String, Instant, Vec<String>)> {
    let map = LIVENESS.lock().unwrap_or_else(|p| p.into_inner());
    map.iter()
        .map(|(id, e)| (id.clone(), e.last_seen, e.manifest_ids.clone()))
        .collect()
}

/// Build and sign a `HeartbeatPing` for this node.
/// `manifest_ids` is the list of manifest IDs this node holds shares for.
pub fn build_heartbeat_ping(local_peer_id: &str, manifest_ids: Vec<String>) -> anyhow::Result<HeartbeatPing> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut ping = HeartbeatPing {
        peer_id: local_peer_id.to_string(),
        timestamp_secs,
        custodian_manifest_ids: manifest_ids,
        sig: String::new(),
    };

    let canonical = ping.canonical_bytes();
    let (_, signing_priv) = crypto::ensure_keypair_on_disk()
        .map_err(|e| anyhow::anyhow!("heartbeat: keypair unavailable: {}", e))?;
    let sig_bytes = crypto::sign_data_with_key(&signing_priv, &canonical)
        .map_err(|e| anyhow::anyhow!("heartbeat: sign failed: {}", e))?;
    ping.sig = crypto::b64_encode(&sig_bytes);
    Ok(ping)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heartbeat_ping_roundtrip() {
        let ping = HeartbeatPing {
            peer_id: "peer-abc".to_string(),
            timestamp_secs: 1_700_000_000,
            custodian_manifest_ids: vec!["manifest-1".to_string(), "manifest-2".to_string()],
            sig: "base64sig".to_string(),
        };
        let bytes = ping.to_bytes();
        let decoded = HeartbeatPing::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.peer_id, "peer-abc");
        assert_eq!(decoded.custodian_manifest_ids.len(), 2);
        assert_eq!(decoded.sig, "base64sig");
    }

    #[test]
    fn test_record_and_retrieve_liveness() {
        let ping = HeartbeatPing {
            peer_id: "test-peer-liveness".to_string(),
            timestamp_secs: 0,
            custodian_manifest_ids: vec!["m1".to_string()],
            sig: String::new(),
        };
        record_heartbeat(&ping);

        let snapshot = liveness_snapshot();
        let entry = snapshot.iter().find(|(id, _, _)| id == "test-peer-liveness");
        assert!(entry.is_some(), "entry should exist after record_heartbeat");
        let (_, _, mids) = entry.unwrap();
        assert_eq!(mids, &["m1"]);
    }

    #[test]
    fn test_expired_custodians_none_initially() {
        // A freshly inserted entry should not be expired
        let ping = HeartbeatPing {
            peer_id: "fresh-peer".to_string(),
            timestamp_secs: 0,
            custodian_manifest_ids: vec![],
            sig: String::new(),
        };
        record_heartbeat(&ping);
        let expired = expired_custodians();
        assert!(
            !expired.iter().any(|(id, _)| id == "fresh-peer"),
            "freshly inserted entry should not be expired"
        );
    }

    #[test]
    fn test_remove_liveness_entry() {
        let ping = HeartbeatPing {
            peer_id: "to-remove-peer".to_string(),
            timestamp_secs: 0,
            custodian_manifest_ids: vec![],
            sig: String::new(),
        };
        record_heartbeat(&ping);
        remove_liveness_entry("to-remove-peer");
        let snap = liveness_snapshot();
        assert!(!snap.iter().any(|(id, _, _)| id == "to-remove-peer"));
    }

    #[test]
    fn test_canonical_bytes_excludes_sig() {
        let ping = HeartbeatPing {
            peer_id: "p".to_string(),
            timestamp_secs: 42,
            custodian_manifest_ids: vec![],
            sig: "some-sig".to_string(),
        };
        let canonical = ping.canonical_bytes();
        // Canonical bytes must not include the sig — verify by comparing with empty-sig version
        let no_sig = HeartbeatPing { sig: String::new(), ..ping.clone() };
        assert_eq!(canonical, no_sig.canonical_bytes());
        // And must differ from full bytes
        assert_ne!(canonical, ping.to_bytes());
    }
}
