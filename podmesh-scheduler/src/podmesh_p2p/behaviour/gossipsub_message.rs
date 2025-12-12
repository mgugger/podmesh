use super::message_verifier::verify_signed_message;
use crate::podmesh_p2p::{capacity, utils};
use crate::resource_verifier::{CapacityCheckResult, ResourceRequest};
use crate::workload_integration::get_global_resource_verifier;
use libp2p::gossipsub;
use log::{debug, error, info, warn};
use lru::LruCache;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use protocol::libp2p_constants::FREE_CAPACITY_TIMEOUT_MS;
use std::num::NonZeroUsize;

/// Maximum number of seen capacity request IDs to track.
/// This prevents unbounded memory growth while still catching recent duplicates.
const SEEN_CAPREQ_CACHE_SIZE: usize = 10_000;

/// Global LRU cache tracking recently seen capacity request IDs.
/// This prevents duplicate processing and limits amplification from re-broadcasts.
static SEEN_CAPREQ_CACHE: Lazy<Mutex<LruCache<String, ()>>> = Lazy::new(|| {
    Mutex::new(LruCache::new(
        NonZeroUsize::new(SEEN_CAPREQ_CACHE_SIZE).expect("cache size must be > 0"),
    ))
});

/// Check if we've seen this request ID recently. Returns true if already seen.
fn mark_request_seen(request_id: &str) -> bool {
    let mut cache = SEEN_CAPREQ_CACHE.lock();
    if cache.contains(request_id) {
        true
    } else {
        cache.put(request_id.to_string(), ());
        false
    }
}

/// Forward a capacity request to peers with decremented hop count.
/// This enables mesh-wide discovery while limiting amplification attacks.
fn forward_capacity_request(
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
    topic: &gossipsub::TopicHash,
    cap_req: &protocol::machine::CapacityRequest,
    new_hops: u8,
    request_id: &str,
) {
    // Build a new capacity request with the decremented hop count, preserving owner_pubkey
    let forwarded_payload = protocol::machine::build_capacity_request_full(
        request_id,
        cap_req.cpu_milli,
        cap_req.memory_bytes,
        cap_req.storage_bytes,
        cap_req.replicas,
        new_hops,
        &cap_req.owner_pubkey,
    );

    // Sign and publish the forwarded request
    match utils::sign_payload_default(&forwarded_payload, "capacity_request", Some("capreq_fwd")) {
        Ok(signed_bytes) => {
            match swarm
                .behaviour_mut()
                .gossipsub
                .publish(topic.clone(), signed_bytes)
            {
                Ok(_) => {
                    debug!(
                        "libp2p: forwarded capreq id={} with {} hops remaining",
                        request_id, new_hops
                    );
                }
                Err(e) => {
                    warn!(
                        "libp2p: failed to forward capreq id={}: {:?}",
                        request_id, e
                    );
                }
            }
        }
        Err(e) => {
            warn!(
                "libp2p: failed to sign forwarded capreq id={}: {}",
                request_id, e
            );
        }
    }
}

pub fn gossipsub_message(
    peer_id: libp2p::PeerId,
    message: gossipsub::Message,
    topic: gossipsub::TopicHash,
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
    pending_queries: &mut std::collections::HashMap<
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
        pubkey: requester_pubkey,
        ..
    } = verified;
    
    // Convert requester's pubkey to base64 for storage in reservation
    let requester_pubkey_b64 = crypto::b64_encode(&requester_pubkey);

    // Then try CapacityReply
    if let Ok(cap_req) = protocol::machine::root_as_capacity_request(payload.as_slice()) {
        let orig_request_id = cap_req.request_id().unwrap_or("").to_string();
        let responder_peer = swarm.local_peer_id().to_string();
        let remaining_hops = cap_req.max_hops;

        // Check if we've already processed this request (prevent loops and duplicates)
        if mark_request_seen(&orig_request_id) {
            debug!(
                "libp2p: ignoring duplicate capreq id={} from peer={}",
                orig_request_id, peer_id
            );
            return;
        }

        let age_ms = utils::make_timestamp_ms().saturating_sub(timestamp_ms);
        if age_ms > FREE_CAPACITY_TIMEOUT_MS {
            warn!(
                "libp2p: dropping stale capreq id={} age={}ms from peer={}",
                orig_request_id, age_ms, peer_id
            );
            return;
        }

        // Forward the request with decremented hop count if hops remain
        if remaining_hops > 0 {
            forward_capacity_request(
                swarm,
                &topic,
                &cap_req,
                remaining_hops.saturating_sub(1),
                &orig_request_id,
            );
        }

        let manifest_id = match utils::extract_manifest_id_from_request_id(&orig_request_id) {
            Some(id) => id,
            None => {
                warn!(
                    "libp2p: capreq id={} missing manifest id, ignoring",
                    orig_request_id
                );

                return;
            }
        };

        let resource_request = ResourceRequest::new(
            Some(cap_req.cpu_milli()),
            Some(cap_req.memory_bytes()),
            Some(cap_req.storage_bytes()),
            cap_req.replicas(),
        );

        info!(
            "libp2p: received capreq id={} manifest_id={} from peer={} payload_bytes={}",
            orig_request_id,
            manifest_id,
            peer_id,
            payload.len()
        );

        let verifier = get_global_resource_verifier();

        // Perform synchronous capacity check using cached resources.
        let request_id_for_check = orig_request_id.clone();
        let verifier_for_check = verifier.clone();
        let resource_request_for_check = resource_request.clone();
        let check_handle = tokio::runtime::Handle::current();
        let check_result = std::thread::spawn(move || {
            check_handle.block_on(verifier_for_check.verify_capacity(&resource_request_for_check))
        })
        .join()
        .unwrap_or_else(|_| {
            warn!(
                "libp2p: capacity check thread panicked for request_id={}",
                request_id_for_check
            );
            CapacityCheckResult {
                has_capacity: false,
                rejection_reason: Some("internal error".to_string()),
                available_cpu_milli: 0,
                available_memory_bytes: 0,
                available_storage_bytes: 0,
            }
        });

        if !check_result.has_capacity {
            info!(
                "libp2p: capacity check failed for request_id={} manifest_id={} reason={:?}",
                orig_request_id, manifest_id, check_result.rejection_reason
            );
            return;
        }

        // Reserve resources for a short period to back the bid.
        // Use the owner_pubkey from the capacity request if present (this is the CLI's pubkey),
        // otherwise fall back to the envelope sender's pubkey for backward compatibility.
        let reserve_request_id = orig_request_id.clone();
        let reserve_manifest_id = manifest_id.clone();
        let reserve_owner_pubkey = if !cap_req.owner_pubkey.is_empty() {
            cap_req.owner_pubkey.clone()
        } else {
            requester_pubkey_b64.clone()
        };
        let verifier_for_reserve = verifier.clone();
        let resource_request_for_reserve = resource_request.clone();
        let reserve_handle = tokio::runtime::Handle::current();
        let reserve_outcome = std::thread::spawn(move || {
            reserve_handle.block_on(verifier_for_reserve.reserve_capacity(
                &reserve_request_id,
                Some(reserve_manifest_id.as_str()),
                Some(reserve_owner_pubkey.as_str()),
                &resource_request_for_reserve,
            ))
        })
        .join();

        match reserve_outcome {
            Ok(Ok(())) => {
                info!(
                    "libp2p: reserved resources for request_id={} manifest_id={}",
                    orig_request_id, manifest_id
                );
            }
            Ok(Err(err)) => {
                warn!(
                    "libp2p: failed to reserve resources for request_id={} manifest_id={}: {}",
                    orig_request_id, manifest_id, err
                );
                return;
            }
            Err(_) => {
                warn!(
                    "libp2p: reservation thread panicked for request_id={} manifest_id={}",
                    orig_request_id, manifest_id
                );
                return;
            }
        }

        let reply = capacity::compose_capacity_reply(
            "gossipsub",
            &orig_request_id,
            &responder_peer,
            |params| {
                params.ok = true;
                params.cpu_milli = check_result.available_cpu_milli;
                params.memory_bytes = check_result.available_memory_bytes;
                params.storage_bytes = check_result.available_storage_bytes;
            },
        );
        let payload_len = reply.payload.len();

        match capacity::publish_gossipsub_capacity_reply(
            &mut swarm.behaviour_mut().gossipsub,
            &topic,
            &reply,
        ) {
            Ok(_) => {
                info!(
                    "libp2p: published capreply for id={} manifest_id={} ({} bytes)",
                    orig_request_id, manifest_id, payload_len
                );
            }
            Err(e) => {
                error!(
                    "libp2p: failed to publish signed capacity reply id={} to {}: {:?}",
                    orig_request_id, peer_id, e
                );
            }
        }
        return;
    }

    if let Ok(cap_reply) = protocol::machine::root_as_capacity_reply(payload.as_slice()) {
        let request_part = cap_reply.request_id().unwrap_or("").to_string();
        info!(
            "libp2p: received capreply for id={} from peer={}",
            request_part, peer_id
        );
        // Extract KEM pubkey from capacity reply and include it in the notification
        let peer_pubkey = cap_reply.kem_pubkey().unwrap_or("");
        let peer_with_key = format!("{}:{}", peer_id.to_string(), peer_pubkey);
        utils::notify_capacity_observers(pending_queries, &request_part, move || {
            peer_with_key.clone()
        });
        return;
    }

    warn!(
        "gossipsub: Received unsupported message ({} bytes) from peer {}",
        payload.len(),
        peer_id
    );
}
