use super::message_verifier::verify_signed_message;
use crate::podmesh_p2p::utils;
use libp2p::gossipsub;
use log::{debug, info, warn};
use lru::LruCache;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use protocol::libp2p_constants::FREE_CAPACITY_TIMEOUT_MS;
use std::num::NonZeroUsize;

/// Load the node cert from the default key directory (~/.podmesh).
fn load_local_node_cert() -> anyhow::Result<Option<protocol::node_cert::NodeCert>> {
    let key_dir = {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("no home dir"))?;
        home.join(crypto::KEY_DIR)
    };
    protocol::node_cert::load_node_cert(key_dir.to_str().unwrap_or(".podmesh"))
}

/// Maximum number of seen capability query IDs to track.
const SEEN_CAPQ_CACHE_SIZE: usize = 10_000;

/// Global LRU cache tracking recently seen capability query IDs.
static SEEN_CAPQ_CACHE: Lazy<Mutex<LruCache<String, ()>>> = Lazy::new(|| {
    Mutex::new(LruCache::new(
        NonZeroUsize::new(SEEN_CAPQ_CACHE_SIZE).expect("cache size must be > 0"),
    ))
});

/// Check if we've seen this query ID recently. Returns true if already seen.
fn mark_query_seen(query_id: &str) -> bool {
    let mut cache = SEEN_CAPQ_CACHE.lock();
    if cache.contains(query_id) {
        true
    } else {
        cache.put(query_id.to_string(), ());
        false
    }
}

/// Forward a capability query to peers with decremented hop count.
fn forward_capability_query(
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
    topic: &gossipsub::TopicHash,
    cap_q: &protocol::machine::CapabilityQuery,
    new_hops: u8,
) {
    // Rebuild the query with updated hop count
    let with_hops = protocol::machine::CapabilityQuery {
        query_id: cap_q.query_id.clone(),
        nonce: cap_q.nonce.clone(),
        required_capabilities: cap_q.required_capabilities.clone(),
        role_filter: cap_q.role_filter.clone(),
        initiator_pubkey: cap_q.initiator_pubkey.clone(),
        max_hops: new_hops,
    };
    let forwarded_bytes = protocol::machine::serialize_capability_query(&with_hops);

    match utils::sign_payload_default(&forwarded_bytes, "capability_query", Some("capq_fwd")) {
        Ok(signed_bytes) => {
            match swarm
                .behaviour_mut()
                .gossipsub
                .publish(topic.clone(), signed_bytes)
            {
                Ok(_) => {
                    debug!(
                        "libp2p: forwarded capq id={} with {} hops remaining",
                        cap_q.query_id, new_hops
                    );
                }
                Err(e) => {
                    warn!("libp2p: failed to forward capq id={}: {:?}", cap_q.query_id, e);
                }
            }
        }
        Err(e) => {
            warn!("libp2p: failed to sign forwarded capq id={}: {}", cap_q.query_id, e);
        }
    }
}

/// Check if this node satisfies the capability query's role and capability requirements.
fn node_satisfies_capability_query(cap_q: &protocol::machine::CapabilityQuery) -> bool {
    // Role check: if role_filter is non-empty, verify our node cert role matches
    if !cap_q.role_filter.is_empty() {
        if let Ok(Some(cert)) = load_local_node_cert() {
            let required_role: Result<protocol::node_cert::NodeRole, _> =
                cap_q.role_filter.parse();
            if let Ok(role) = required_role {
                if !cert.has_role(&role) {
                    debug!(
                        "libp2p: capq id={} role_filter={} not satisfied by our role={}",
                        cap_q.query_id, cap_q.role_filter, cert.role
                    );
                    return false;
                }
            }
        }
        // If no node cert is present, we are effectively NodeRole::Both (default)
    }

    // Capability check: if required_capabilities is non-empty, check subset match
    if !cap_q.required_capabilities.is_empty() {
        if let Ok(Some(cert)) = load_local_node_cert() {
            let our_caps: std::collections::HashSet<&str> =
                cert.capabilities.iter().map(|s| s.as_str()).collect();
            for req in &cap_q.required_capabilities {
                if !our_caps.contains(req.as_str()) {
                    debug!(
                        "libp2p: capq id={} required capability '{}' not found",
                        cap_q.query_id, req
                    );
                    return false;
                }
            }
        }
        // No cert: treat as having default capabilities only; fail if specific caps required
        // unless the required list is exactly ["default"]
        else {
            let only_default = cap_q.required_capabilities == vec!["default"];
            if !only_default {
                return false;
            }
        }
    }

    true
}

/// Sign `payload` bytes and publish them to a gossipsub topic.
/// Returns `Ok(())` on success.
pub fn sign_and_publish(
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
    topic: &gossipsub::IdentTopic,
    payload: &[u8],
    label: &str,
) -> anyhow::Result<()> {
    use crate::podmesh_p2p::utils;
    let signed = utils::sign_payload_default(payload, label, Some(label))
        .map_err(|e| anyhow::anyhow!("sign failed: {}", e))?;
    swarm
        .behaviour_mut()
        .gossipsub
        .publish(topic.clone(), signed)
        .map_err(|e| anyhow::anyhow!("publish failed: {:?}", e))?;
    Ok(())
}

pub fn gossipsub_message(
    peer_id: libp2p::PeerId,
    message: gossipsub::Message,
    topic: gossipsub::TopicHash,
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
    _pending_queries: &mut std::collections::HashMap<
        String,
        Vec<tokio::sync::mpsc::UnboundedSender<String>>,
    >,
) {
    debug!("received message from {}", peer_id);

    let verified = match verify_signed_message(&peer_id, &message.data, |err| {
        warn!("gossipsub: rejecting message from {}: {}", peer_id, err);
    }) {
        Some(envelope) => envelope,
        None => return,
    };
    let crate::podmesh_p2p::security::VerifiedEnvelope {
        payload,
        timestamp_ms,
        ..
    } = verified;

    if let Ok(cap_q) = protocol::machine::root_as_capability_query(payload.as_slice()) {
        let query_id = cap_q.query_id.clone();
        let remaining_hops = cap_q.max_hops;

        // Deduplicate
        if mark_query_seen(&query_id) {
            debug!(
                "libp2p: ignoring duplicate capq id={} from peer={}",
                query_id, peer_id
            );
            return;
        }

        let age_ms = utils::make_timestamp_ms().saturating_sub(timestamp_ms);
        if age_ms > FREE_CAPACITY_TIMEOUT_MS {
            warn!(
                "libp2p: dropping stale capq id={} age={}ms from peer={}",
                query_id, age_ms, peer_id
            );
            return;
        }

        // Forward with decremented hop count
        if remaining_hops > 0 {
            forward_capability_query(
                swarm,
                &topic,
                &cap_q,
                remaining_hops.saturating_sub(1),
            );
        }

        info!(
            "libp2p: received capq id={} from peer={} role_filter={} caps={:?}",
            query_id, peer_id, cap_q.role_filter, cap_q.required_capabilities
        );

        // Evaluate whether we satisfy the query
        if crate::podmesh_p2p::is_scheduling_disabled_for(swarm.local_peer_id()) {
            debug!("libp2p: scheduling disabled, skipping capq id={}", query_id);
            return;
        }

        if !node_satisfies_capability_query(&cap_q) {
            debug!("libp2p: capq id={} not satisfied by this node", query_id);
            return;
        }

        // Build and send a CapabilityReply directly to the initiator via scheduler_rr.
        // We need to send to the peer that originated the query (peer_id), not who forwarded it.
        // Since gossipsub doesn't expose the original sender when forwarded, we send to
        // the gossipsub message source (propagation_source) for now.
        let local_peer = swarm.local_peer_id().to_string();
        let kem_pub_b64 = crypto::ensure_kem_keypair_on_disk()
            .ok()
            .map(|(pub_bytes, _)| crypto::b64_encode(&pub_bytes))
            .unwrap_or_default();

        let (node_cert_bytes, capabilities, role) =
            match load_local_node_cert() {
                Ok(Some(cert)) => {
                    let role = cert.role.to_string();
                    let caps: Vec<String> = cert.capabilities.clone();
                    let bytes = cert.to_bytes();
                    (bytes, caps, role)
                }
                _ => (vec![], vec!["default".to_string()], "both".to_string()),
            };

        let caps_ref: Vec<&str> = capabilities.iter().map(|s| s.as_ref()).collect();
        let reply_payload = protocol::machine::build_capability_reply(
            &query_id,
            &local_peer,
            &kem_pub_b64,
            node_cert_bytes,
            &caps_ref,
            &role,
        );

        // Sign the reply and send it directly via scheduler_rr to the propagation source.
        match utils::sign_payload_default(&reply_payload, "capability_reply", Some("capreply")) {
            Ok(signed) => {
                swarm
                    .behaviour_mut()
                    .scheduler_rr
                    .send_request(&peer_id, signed);
                info!(
                    "libp2p: sent capability reply for capq id={} to peer={}",
                    query_id, peer_id
                );
            }
            Err(e) => {
                warn!(
                    "libp2p: failed to sign capability reply for capq id={}: {}",
                    query_id, e
                );
            }
        }
        return;
    }

    // Phase 6: HeartbeatPing — update liveness tracker
    if let Ok(ping) = protocol::machine::HeartbeatPing::from_bytes(payload.as_slice()) {
        crate::custodian::heartbeat::record_heartbeat(&ping);
        debug!(
            "gossipsub: heartbeat from peer={} manifests={}",
            ping.peer_id,
            ping.custodian_manifest_ids.len()
        );
        return;
    }

    // Phase 6: CustodianWithdraw — remove peer from liveness tracker
    if let Ok(withdraw) = protocol::machine::CustodianWithdraw::from_bytes(payload.as_slice()) {
        info!(
            "gossipsub: CustodianWithdraw for manifest={} from peer={}",
            withdraw.manifest_id, withdraw.custodian_peer_id
        );
        crate::custodian::heartbeat::remove_liveness_entry(&withdraw.custodian_peer_id);
        return;
    }

    warn!(
        "gossipsub: Received unsupported message ({} bytes) from peer {}",
        payload.len(),
        peer_id
    );
}
