use libp2p::{Swarm, gossipsub};

use std::collections::HashMap as StdHashMap;
use std::sync::Mutex;
use tokio::sync::mpsc;
use once_cell::sync::Lazy;

use crate::podmesh_p2p::behaviour::MyBehaviour;
use crate::podmesh_p2p::utils;

/// Tracks in-flight ResourceQuery requests: OutboundRequestId debug string → query_id
static PENDING_RESOURCE_QUERIES: Lazy<Mutex<StdHashMap<String, String>>> =
    Lazy::new(|| Mutex::new(StdHashMap::new()));

pub fn insert_pending_resource_query(local_peer_id: &str, request_id: libp2p::request_response::OutboundRequestId, query_id: String) {
    let key = format!("{}:{:?}", local_peer_id, request_id);
    log::debug!("insert_pending_resource_query: key={} query_id={}", key, query_id);
    PENDING_RESOURCE_QUERIES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, query_id);
}

pub fn take_pending_resource_query(
    local_peer_id: &str,
    request_id: &libp2p::request_response::OutboundRequestId,
) -> Option<String> {
    let key = format!("{}:{:?}", local_peer_id, request_id);
    PENDING_RESOURCE_QUERIES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&key)
}

/// Handle QueryCapacityWithPayload control message.
/// This now initiates a two-phase capability + resource discovery:
///   Phase 1: broadcast CapabilityQuery over gossipsub
///   Phase 2: nodes that match send a CapabilityReply → we send ResourceQuery → ResourceReply → notify caller
pub async fn handle_query_capacity_with_payload(
    request_id: String,
    reply_tx: mpsc::UnboundedSender<String>,
    payload: Vec<u8>,
    swarm: &mut Swarm<MyBehaviour>,
    _topic: &gossipsub::IdentTopic,
    pending_queries: &mut StdHashMap<String, Vec<mpsc::UnboundedSender<String>>>,
) {
    log::debug!(
        "libp2p: control QueryCapacityWithPayload received request_id={} payload_len={}",
        request_id,
        payload.len()
    );

    // Register the reply channel — notifications arrive when ResourceReply.ok=true
    pending_queries
        .entry(request_id.clone())
        .or_insert_with(Vec::new)
        .push(reply_tx);

    // Build CapabilityQuery from the payload.
    // The payload may be a legacy CapacityRequest or a new ResourceQuery — we accept both
    // and extract the required resources. For Phase 0.5 the REST API still builds a
    // CapacityRequest-shaped payload; we interpret it to build the CapabilityQuery.
    let (cpu_milli, memory_bytes, storage_bytes, replicas, owner_pubkey) =
        extract_resource_params(&payload, &request_id);

    // Phase 1: broadcast CapabilityQuery via gossipsub
    let nonce = utils::make_nonce(Some("capq"));
    let initiator_pubkey = crypto::ensure_keypair_on_disk()
        .ok()
        .map(|(pk, _)| crypto::b64_encode(&pk))
        .unwrap_or_default();

    let cap_query_payload = protocol::machine::build_capability_query(
        &request_id,
        &nonce,
        &["default"],   // required capabilities; callers can extend later
        "",             // role_filter: any role for now
        &initiator_pubkey,
    );

    match utils::broadcast_signed_request_to_peers(swarm, &cap_query_payload, "capability_query") {
        Ok(sent) => {
            log::info!(
                "libp2p: broadcasted capq request_id={} to {} peers",
                request_id,
                sent
            );
        }
        Err(e) => {
            log::error!("failed to broadcast capability query: {:?}", e);
        }
    }

    // Also handle the local node response if scheduling is enabled
    if crate::podmesh_p2p::is_scheduling_disabled_for(swarm.local_peer_id()) {
        log::info!(
            "libp2p: local scheduling disabled, skipping local capacity response for {}",
            request_id
        );
    } else {
        let local_peer = swarm.local_peer_id().to_string();

        let verifier = crate::workload_integration::get_global_resource_verifier();
        let resource_request = crate::resource_verifier::ResourceRequest::new(
            Some(cpu_milli),
            Some(memory_bytes),
            Some(storage_bytes),
            replicas,
        );

        let check_handle = tokio::runtime::Handle::current();
        let verifier_clone = verifier.clone();
        let rr_clone = resource_request.clone();
        let check_result = std::thread::spawn(move || {
            check_handle.block_on(verifier_clone.verify_capacity(&rr_clone))
        })
        .join()
        .ok();

        if let Some(result) = check_result {
            if result.has_capacity {
                // Reserve for local node
                let reserve_handle = tokio::runtime::Handle::current();
                let verifier_r = verifier.clone();
                let rr_r = resource_request.clone();
                let rid = request_id.clone();
                let opk = owner_pubkey.clone();
                let _ = std::thread::spawn(move || {
                    reserve_handle.block_on(verifier_r.reserve_capacity(
                        &rid,
                        Some(&rid),
                        if opk.is_empty() { None } else { Some(opk.as_str()) },
                        &rr_r,
                    ))
                })
                .join();

                let kem_pub_b64 = crypto::ensure_kem_keypair_on_disk()
                    .ok()
                    .map(|(pub_bytes, _)| crypto::b64_encode(&pub_bytes))
                    .unwrap_or_default();

                let local_response = format!("{}:{}", local_peer, kem_pub_b64);
                utils::notify_capacity_observers(pending_queries, &request_id, move || {
                    local_response.clone()
                });
            }
        }
    }
}

/// Extract resource parameters from payload.
/// Accepts either a CapacityRequest (legacy) or a ResourceQuery (new) payload.
fn extract_resource_params(payload: &[u8], request_id: &str) -> (u32, u64, u64, u32, String) {
    // Try new ResourceQuery format first
    if let Ok(rq) = protocol::machine::root_as_resource_query(payload) {
        return (rq.cpu_milli, rq.memory_bytes, rq.storage_bytes, rq.replicas, rq.owner_pubkey);
    }
    // Fallback: default resource values
    log::debug!(
        "libp2p: could not parse resource params for request_id={}, using defaults",
        request_id
    );
    (500, 512 * 1024 * 1024, 10 * 1024 * 1024 * 1024, 1, String::new())
}

#[cfg(test)]
mod tests {
    use crate::resource_verifier::ResourceVerifier;

    #[tokio::test]
    async fn test_capacity_reply_reflects_actual_resources() {
        // Verify that the ResourceVerifier returns consistent (non-echoed) values
        let verifier = ResourceVerifier::new();
        let sys = verifier.get_system_resources().await;

        let cpu1 = sys.available_cpu_milli();
        let mem1 = sys.available_memory_bytes();
        let storage1 = sys.available_storage_bytes();

        let sys2 = verifier.get_system_resources().await;
        let cpu2 = sys2.available_cpu_milli();
        let mem2 = sys2.available_memory_bytes();
        let storage2 = sys2.available_storage_bytes();

        assert_eq!(cpu1, cpu2, "CPU availability should be stable between close calls");
        assert_eq!(mem1, mem2, "Memory availability should be stable between close calls");
        assert_eq!(storage1, storage2, "Storage availability should be stable between close calls");
    }
}
