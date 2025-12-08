use super::message_verifier::verify_signed_message;
use crate::podmesh_p2p::capacity;
use crate::podmesh_p2p::utils;
use crate::resource_verifier::ResourceRequest;
use crate::workload_integration::get_global_resource_verifier;
use libp2p::request_response;
use log::{debug, error, info, warn};
use protocol::libp2p_constants::FREE_CAPACITY_TIMEOUT_MS;
use std::collections::HashMap as StdHashMap;
use tokio::sync::mpsc;

pub fn scheduler_message(
    message: request_response::Message<Vec<u8>, Vec<u8>>,
    peer: libp2p::PeerId,
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
    local_peer: libp2p::PeerId,
    pending_queries: &mut StdHashMap<String, Vec<mpsc::UnboundedSender<String>>>,
) {
    match message {
        request_response::Message::Request {
            request, channel, ..
        } => {
            handle_scheduler_request(request, channel, peer, swarm, local_peer);
        }
        request_response::Message::Response { response, .. } => {
            debug!("libp2p: received scheduler response from peer={}", peer);
            if let Ok(cap_reply) = protocol::machine::root_as_capacity_reply(&response) {
                let request_part = cap_reply.request_id().unwrap_or("").to_string();
                debug!(
                    "libp2p: scheduler reply ok={} from {} for request_id={}",
                    cap_reply.ok(),
                    peer,
                    request_part
                );
                // KEM pubkey caching has been removed - keys are now extracted directly from envelopes
                let peer_pubkey = cap_reply.kem_pubkey().unwrap_or("");
                let peer_with_key = format!("{}:{}", peer.to_string(), peer_pubkey);
                utils::notify_capacity_observers(pending_queries, &request_part, move || {
                    peer_with_key.clone()
                });
            }
        }
    }
}

fn handle_scheduler_request(
    request: Vec<u8>,
    channel: request_response::ResponseChannel<Vec<u8>>,
    peer: libp2p::PeerId,
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
    local_peer: libp2p::PeerId,
) {
    if crate::podmesh_p2p::is_scheduling_disabled_for(&local_peer) {
        debug!(
            "libp2p: scheduling disabled, ignoring scheduler request from {}",
            peer
        );
        return;
    }

    debug!("libp2p: received scheduler request from peer={}", peer);

    let verified = match verify_signed_message(&peer, &request, |err| {
        error!("rejecting invalid scheduler request: {}", err);
    }) {
        Some(envelope) => envelope,
        None => return,
    };

    let crate::podmesh_p2p::security::VerifiedEnvelope {
        payload: effective_request,
        timestamp_ms,
        ..
    } = verified;

    let Some(request_ctx) = parse_capacity_request(&effective_request, timestamp_ms, &peer) else {
        return;
    };

    let verifier = get_global_resource_verifier();
    let check_result = run_capacity_check(
        &verifier,
        &request_ctx.resource_request,
        &request_ctx.request_id,
    );

    if !check_result.has_capacity {
        log_capacity_failure(&request_ctx.request_id, &check_result);
        return;
    }

    if !reserve_capacity_for_request(
        &verifier,
        &request_ctx.resource_request,
        &request_ctx.request_id,
        &request_ctx.manifest_id,
    ) {
        return;
    }

    let responder_peer = local_peer.to_string();
    send_capacity_reply(
        swarm,
        channel,
        &request_ctx.request_id,
        &responder_peer,
        &peer,
        &check_result,
    );
}

struct CapacityRequestContext {
    request_id: String,
    manifest_id: String,
    resource_request: ResourceRequest,
}

fn parse_capacity_request(
    effective_request: &[u8],
    timestamp_ms: u64,
    peer: &libp2p::PeerId,
) -> Option<CapacityRequestContext> {
    let cap_req = match protocol::machine::root_as_capacity_request(effective_request) {
        Ok(req) => req,
        Err(e) => {
            warn!("libp2p: failed to parse scheduler request: {:?}", e);
            return None;
        }
    };

    let orig_request_id = cap_req.request_id().unwrap_or("");
    let age_ms = utils::make_timestamp_ms().saturating_sub(timestamp_ms);
    if age_ms > FREE_CAPACITY_TIMEOUT_MS {
        warn!(
            "libp2p: dropping stale scheduler capreq id={} age={}ms from {}",
            orig_request_id, age_ms, peer
        );
        return None;
    }

    let manifest_id = match utils::extract_manifest_id_from_request_id(orig_request_id) {
        Some(id) => id,
        None => {
            warn!(
                "libp2p: scheduler capreq id={} missing manifest id, ignoring",
                orig_request_id
            );
            return None;
        }
    };

    debug!(
        "libp2p: scheduler capacity request id={} manifest_id={} from {}",
        orig_request_id, manifest_id, peer
    );

    let resource_request = ResourceRequest::new(
        Some(cap_req.cpu_milli()),
        Some(cap_req.memory_bytes()),
        Some(cap_req.storage_bytes()),
        cap_req.replicas(),
    );

    Some(CapacityRequestContext {
        request_id: orig_request_id.into(),
        manifest_id,
        resource_request,
    })
}

fn run_capacity_check(
    verifier: &std::sync::Arc<crate::resource_verifier::ResourceVerifier>,
    resource_request: &ResourceRequest,
    request_id: &str,
) -> crate::resource_verifier::CapacityCheckResult {
    let handle = tokio::runtime::Handle::current();
    let verifier_for_check = verifier.clone();
    let resource_request_for_check = resource_request.clone();

    std::thread::spawn(move || {
        handle.block_on(verifier_for_check.verify_capacity(&resource_request_for_check))
    })
    .join()
    .unwrap_or_else(|_| {
        warn!(
            "Capacity check thread panicked for request_id={}; assuming no capacity",
            request_id
        );
        crate::resource_verifier::CapacityCheckResult {
            has_capacity: false,
            rejection_reason: Some("Internal error".into()),
            available_cpu_milli: 0,
            available_memory_bytes: 0,
            available_storage_bytes: 0,
        }
    })
}

fn reserve_capacity_for_request(
    verifier: &std::sync::Arc<crate::resource_verifier::ResourceVerifier>,
    resource_request: &ResourceRequest,
    request_id: &str,
    manifest_id: &str,
) -> bool {
    let reserve_handle = tokio::runtime::Handle::current();
    let verifier_clone = verifier.clone();
    let request_clone = resource_request.clone();
    let reserve_request_id = request_id.to_string();
    let reserve_manifest_id = manifest_id.to_string();
    let reservation = std::thread::spawn(move || {
        reserve_handle.block_on(verifier_clone.reserve_capacity(
            &reserve_request_id,
            Some(reserve_manifest_id.as_str()),
            &request_clone,
        ))
    })
    .join();

    match reservation {
        Ok(Ok(())) => {
            debug!(
                "libp2p: reserved resources for request_id={} manifest_id={}",
                request_id, manifest_id
            );
            true
        }
        Ok(Err(err)) => {
            warn!(
                "libp2p: failed to reserve resources for request_id={} manifest_id={}: {}",
                request_id, manifest_id, err
            );
            false
        }
        Err(_) => {
            warn!(
                "libp2p: reservation thread panicked for request_id={} manifest_id={}",
                request_id, manifest_id
            );
            false
        }
    }
}

fn send_capacity_reply(
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
    channel: request_response::ResponseChannel<Vec<u8>>,
    request_id: &str,
    responder_peer: &str,
    remote_peer: &libp2p::PeerId,
    check_result: &crate::resource_verifier::CapacityCheckResult,
) {
    let reply =
        capacity::compose_capacity_reply("scheduler", request_id, responder_peer, |params| {
            params.ok = true;
            params.cpu_milli = check_result.available_cpu_milli;
            params.memory_bytes = check_result.available_memory_bytes;
            params.storage_bytes = check_result.available_storage_bytes;
        });

    let payload_len = reply.payload.len();
    match capacity::send_scheduler_capacity_reply(
        &mut swarm.behaviour_mut().scheduler_rr,
        channel,
        reply,
    ) {
        Ok(_) => {
            debug!(
                "libp2p: sent scheduler capacity reply for id={} to {} ({} bytes)",
                request_id, remote_peer, payload_len
            );
        }
        Err(e) => {
            error!(
                "libp2p: failed to send scheduler capacity reply for id={} to {}: {:?}",
                request_id, remote_peer, e
            );
        }
    }
}

fn log_capacity_failure(
    request_id: &str,
    check_result: &crate::resource_verifier::CapacityCheckResult,
) {
    info!(
        "Capacity check failed for request_id={}: {} - not sending response",
        request_id,
        check_result
            .rejection_reason
            .clone()
            .unwrap_or_else(|| "Unknown reason".into())
    );
}
