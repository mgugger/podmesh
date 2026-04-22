use super::message_verifier::verify_signed_message;
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
            handle_scheduler_request(request, channel, peer, swarm, local_peer, pending_queries);
        }
        request_response::Message::Response { response, request_id, .. } => {
            debug!("libp2p: received scheduler response from peer={}", peer);
            handle_scheduler_response(response, request_id, peer, swarm, pending_queries);
        }
    }
}

/// Handle incoming scheduler request. Two message types are expected:
/// - `CapabilityReply`: a node replied to our gossipsub CapabilityQuery — deliver to waiting callers
/// - `ResourceQuery`: an initiator wants to know our actual resource availability
fn handle_scheduler_request(
    request: Vec<u8>,
    channel: request_response::ResponseChannel<Vec<u8>>,
    peer: libp2p::PeerId,
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
    local_peer: libp2p::PeerId,
    _pending_queries: &mut StdHashMap<String, Vec<mpsc::UnboundedSender<String>>>,
) {
    let verified = match verify_signed_message(&peer, &request, |err| {
        error!("rejecting invalid scheduler request: {}", err);
    }) {
        Some(envelope) => envelope,
        None => return,
    };

    let crate::podmesh_p2p::security::VerifiedEnvelope {
        payload,
        timestamp_ms,
        payload_type,
        ..
    } = verified;

    // Route based on payload_type to avoid ambiguous FlatBuffer/postcard parsing.
    // postcard types (ShareRequest, WorkloadAssignmentV2, WorkloadDispatch) must be
    // checked by payload_type before trying FlatBuffer parsers, because postcard bytes
    // can accidentally satisfy a FlatBuffer parser and be mis-handled.

    if payload_type == "share_request" {
        if let Ok(share_req) = crypto::ShareRequest::from_bytes(&payload) {
            log::warn!("handle_scheduler_request: parsed ShareRequest for manifest_id={} from peer={}", share_req.manifest_id, peer);
            handle_share_request(share_req, channel, peer, swarm);
        } else {
            log::warn!("handle_scheduler_request: payload_type=share_request but failed to parse as ShareRequest ({} bytes) from peer={}", payload.len(), peer);
            let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
        }
        return;
    }

    if payload_type == "workload_assignment_v2" {
        if let Ok(assignment_v2) = protocol::machine::WorkloadAssignmentV2::from_bytes(&payload) {
            handle_workload_assignment_v2(assignment_v2, channel, peer, swarm);
        } else {
            log::warn!("handle_scheduler_request: payload_type=workload_assignment_v2 but failed to parse ({} bytes) from peer={}", payload.len(), peer);
            let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
        }
        return;
    }

    if payload_type == "workload_dispatch" {
        if let Ok(dispatch) = protocol::machine::WorkloadDispatch::from_bytes(&payload) {
            handle_workload_dispatch(dispatch, channel, peer, swarm);
        } else {
            log::warn!("handle_scheduler_request: payload_type=workload_dispatch but failed to parse ({} bytes) from peer={}", payload.len(), peer);
            let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
        }
        return;
    }

    // Try to parse as CapabilityQuery: the initiator broadcasts this to find eligible nodes.
    // When received via scheduler RR, we evaluate the query and respond inline.
    if let Ok(cap_q) = protocol::machine::root_as_capability_query(&payload) {
        let query_id = cap_q.query_id.clone();
        debug!(
            "libp2p: scheduler_rr received capq id={} from peer={}",
            query_id, peer
        );
        if crate::podmesh_p2p::is_scheduling_disabled_for(&local_peer) {
            let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
            return;
        }
        if node_satisfies_capability_query_from_cert(&cap_q) {
            let kem_pub_b64 = crypto::ensure_kem_keypair_on_disk()
                .ok()
                .map(|(pub_bytes, _)| crypto::b64_encode(&pub_bytes))
                .unwrap_or_default();
            let (node_cert_bytes, capabilities, role) = load_local_node_cert_info();
            let caps_ref: Vec<&str> = capabilities.iter().map(|s| s.as_ref()).collect();
            let reply_payload = protocol::machine::build_capability_reply(
                &query_id,
                &local_peer.to_string(),
                &kem_pub_b64,
                node_cert_bytes,
                &caps_ref,
                &role,
            );
            match utils::sign_payload_default(&reply_payload, "capability_reply", Some("capreply")) {
                Ok(signed) => {
                    let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, signed);
                    info!(
                        "libp2p: sent capability reply (inline) for capq id={} to peer={}",
                        query_id, peer
                    );
                }
                Err(e) => {
                    warn!("libp2p: failed to sign inline capability reply for id={}: {}", query_id, e);
                    let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
                }
            }
        } else {
            // Not eligible — send empty ack
            let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
            debug!("libp2p: capq id={} not satisfied by this node, sent empty ack", query_id);
        }
        return;
    }

    // Try to parse as CapabilityReply (Phase 0.5 Step 1: node answers our broadcast — received as request via RR)
    if let Ok(cap_reply) = protocol::machine::root_as_capability_reply(&payload) {
        let query_id = cap_reply.query_id.clone();
        info!(
            "libp2p: received capability reply for query_id={} from peer={} kem={}",
            query_id, peer, cap_reply.kem_pubkey
        );
        // Ack the CapabilityReply immediately
        let _ = swarm
            .behaviour_mut()
            .scheduler_rr
            .send_response(channel, vec![]);

        // If this reply is for a custodian discovery query, forward to the sealing handler.
        if query_id.starts_with("seal:") || query_id.starts_with("custodians:") {
            crate::podmesh_p2p::control::notify_custodian_candidate(
                &query_id,
                crate::custodian::sealer::CustodianCandidate {
                    peer_id: peer.to_string(),
                    kem_pubkey_b64: cap_reply.kem_pubkey.clone(),
                },
            );
            return;
        }

        // Phase 2: Send a ResourceQuery directly to this peer to get actual resource availability.
        // We use default resource requirements; more specific requirements will be added in Phase 5.
        let initiator_pubkey = crypto::ensure_keypair_on_disk()
            .ok()
            .map(|(pk, _)| crypto::b64_encode(&pk))
            .unwrap_or_default();
        let res_query = protocol::machine::build_resource_query(
            &query_id,
            500,
            512 * 1024 * 1024,
            10 * 1024 * 1024 * 1024,
            1,
            &cap_reply.role,
            &initiator_pubkey,
        );
        match utils::sign_payload_default(&res_query, "resource_query", Some("resq")) {
            Ok(signed) => {
                let out_id = swarm.behaviour_mut().scheduler_rr.send_request(&peer, signed);
                // Track the outbound request ID → query_id so we can correlate the ResourceReply
                crate::podmesh_p2p::control::insert_pending_resource_query(&swarm.local_peer_id().to_string(), out_id, query_id.clone());
                debug!("libp2p: sent resource query id={} to peer={}", query_id, peer);
            }
            Err(e) => {
                warn!("libp2p: failed to sign resource query for id={}: {}", query_id, e);
            }
        }
        return;
    }

    // Try to parse as ResourceQuery (Phase 0.5 Step 2: initiator asks for actual resources)
    if let Ok(res_q) = protocol::machine::root_as_resource_query(&payload) {
        handle_resource_query(res_q, channel, peer, swarm, local_peer, timestamp_ms);
        return;
    }

    warn!(
        "libp2p: scheduler_rr: unrecognized request type={} ({} bytes) from peer={}",
        payload_type,
        payload.len(),
        peer
    );
    let _ = swarm
        .behaviour_mut()
        .scheduler_rr
        .send_response(channel, vec![]);
}

fn handle_resource_query(
    res_q: protocol::machine::ResourceQuery,
    channel: request_response::ResponseChannel<Vec<u8>>,
    peer: libp2p::PeerId,
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
    local_peer: libp2p::PeerId,
    timestamp_ms: u64,
) {
    if crate::podmesh_p2p::is_scheduling_disabled_for(&local_peer) {
        debug!(
            "libp2p: scheduling disabled, ignoring resource query from {}",
            peer
        );
        let _ = swarm
            .behaviour_mut()
            .scheduler_rr
            .send_response(channel, vec![]);
        return;
    }

    let age_ms = utils::make_timestamp_ms().saturating_sub(timestamp_ms);
    if age_ms > FREE_CAPACITY_TIMEOUT_MS {
        warn!(
            "libp2p: dropping stale resource query id={} age={}ms from {}",
            res_q.query_id, age_ms, peer
        );
        return;
    }

    debug!(
        "libp2p: resource query id={} cpu={} mem={} from peer={}",
        res_q.query_id, res_q.cpu_milli, res_q.memory_bytes, peer
    );

    let resource_request = ResourceRequest::new(
        Some(res_q.cpu_milli),
        Some(res_q.memory_bytes),
        Some(res_q.storage_bytes),
        res_q.replicas,
    );

    let verifier = get_global_resource_verifier();
    let check_result = run_capacity_check(&verifier, &resource_request, &res_q.query_id);

    if !check_result.has_capacity {
        info!(
            "libp2p: capacity insufficient for resource query id={} reason={:?} — sending ok=false",
            res_q.query_id, check_result.rejection_reason
        );
        let reply = build_resource_reply_payload(&res_q.query_id, &local_peer.to_string(), false, &check_result,
            check_result.rejection_reason.as_deref().unwrap_or("insufficient capacity"));
        let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, reply);
        return;
    }

    // Reserve resources
    // Extract manifest_id from query_id (format: "podmesh-free-capacity:<manifest_id>:<uuid>")
    let manifest_id = utils::extract_manifest_id_from_request_id(&res_q.query_id)
        .unwrap_or_else(|| res_q.query_id.clone());
    let owner_pubkey = if res_q.owner_pubkey.is_empty() { None } else { Some(res_q.owner_pubkey.as_str()) };
    if !reserve_capacity_for_request(&verifier, &resource_request, &res_q.query_id, &manifest_id, owner_pubkey) {
        let reply = build_resource_reply_payload(&res_q.query_id, &local_peer.to_string(), false, &check_result, "reservation failed");
        let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, reply);
        return;
    }

    let reply = build_resource_reply_payload(&res_q.query_id, &local_peer.to_string(), true, &check_result, "");
    let payload_len = reply.len();
    match swarm.behaviour_mut().scheduler_rr.send_response(channel, reply) {
        Ok(_) => {
            debug!(
                "libp2p: sent resource reply for id={} to {} ({} bytes)",
                res_q.query_id, peer, payload_len
            );
        }
        Err(e) => {
            error!(
                "libp2p: failed to send resource reply for id={} to {}: {:?}",
                res_q.query_id, peer, e
            );
        }
    }
}

fn build_resource_reply_payload(
    query_id: &str,
    node_id: &str,
    ok: bool,
    check_result: &crate::resource_verifier::CapacityCheckResult,
    rejection_reason: &str,
) -> Vec<u8> {
    let kem_pub_b64 = crypto::ensure_kem_keypair_on_disk()
        .ok()
        .map(|(pub_bytes, _)| crypto::b64_encode(&pub_bytes))
        .unwrap_or_default();

    let node_cert_bytes = load_local_node_cert_bytes();

    let payload = protocol::machine::build_resource_reply(
        query_id,
        ok,
        node_id,
        &kem_pub_b64,
        check_result.available_cpu_milli,
        check_result.available_memory_bytes,
        check_result.available_storage_bytes,
        rejection_reason,
        node_cert_bytes,
    );

    // Wrap in signed envelope
    match utils::sign_payload_default(&payload, "resource_reply", Some("resreply")) {
        Ok(signed) => signed,
        Err(e) => {
            warn!("libp2p: failed to sign resource reply: {}", e);
            vec![]
        }
    }
}

/// Handle a `WorkloadAssignmentV2` from the coordinator (Shamir secret sharing).
/// This node stores the DEK share record in `CustodianStore`.
fn handle_workload_assignment_v2(
    assignment: protocol::machine::WorkloadAssignmentV2,
    channel: libp2p::request_response::ResponseChannel<Vec<u8>>,
    peer: libp2p::PeerId,
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
) {
    let manifest_id = &assignment.sealed_spec.manifest_id;

    if !crate::podmesh_p2p::get_node_mode().is_custodian() {
        log::warn!(
            "libp2p: received WorkloadAssignmentV2 for {} but we are not in custodian mode",
            manifest_id
        );
        let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
        return;
    }

    let store = match crate::storage::get_custodian_store() {
        Some(s) => s,
        None => {
            log::warn!(
                "libp2p: received WorkloadAssignmentV2 for {} but custodian store not initialized",
                manifest_id
            );
            let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
            return;
        }
    };

    let local_peer = swarm.local_peer_id().to_string();
    let spec = &assignment.sealed_spec;
    let record = crate::storage::CustodianRecord::new(
        manifest_id.clone(),
        spec.owner_pubkey.clone(),
        spec.kfrag_count,
        spec.kfrag_threshold,
        assignment.kfrag_index,
        assignment.wrapped_kfrag.clone(),
        assignment.all_custodian_peers.clone(),
    ).with_coordinator_pubkey(assignment.coordinator_pubkey.clone())
     .with_local_peer_id(local_peer.clone());

    if let Err(e) = store.set_record(&record) {
        log::error!("libp2p: failed to persist kfrag record for {}: {}", manifest_id, e);
        let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
        return;
    }

    log::info!(
        "libp2p: stored kfrag record for manifest_id={} kfrag_index={} from coordinator={}",
        manifest_id, assignment.kfrag_index, peer
    );

    // Ack the assignment
    let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, b"ok".to_vec());

    // If this node is the elected coordinator, kick off worker discovery.
    if crate::custodian::coordinator::is_coordinator(
        manifest_id,
        &local_peer,
        &assignment.all_custodian_peers,
    ) {
        log::info!(
            "libp2p: we are coordinator (v2) for manifest_id={}, triggering worker dispatch",
            manifest_id
        );
        crate::podmesh_p2p::control::enqueue_control(
            &local_peer,
            crate::podmesh_p2p::control::Libp2pControl::DispatchToWorker {
                sealed_spec: assignment.sealed_spec.clone(),
                all_custodian_peers: assignment.all_custodian_peers.clone(),
                required_capabilities: assignment.required_capabilities.clone(),
                replica_count: assignment.sealed_spec.replica_count,
            },
        );
    }
}

/// Handle a `ShareRequest` from a worker (Phase 5/8).
/// Checks whether the manifest is v2 (Umbral PRE, KfragStore) or v1 (Shamir, CustodianStore)
/// and dispatches to the appropriate oracle.
fn handle_share_request(
    share_req: crypto::ShareRequest,
    channel: libp2p::request_response::ResponseChannel<Vec<u8>>,
    peer: libp2p::PeerId,
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
) {
    if !crate::podmesh_p2p::get_node_mode().is_custodian() {
        warn!("libp2p: received ShareRequest from {} but we are not a custodian", peer);
        let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
        return;
    }

    // Use the ShamirOracle (Shamir secret sharing is the only supported sealing mode).
    let is_v1 = crate::storage::get_custodian_store()
        .and_then(|s| s.get_record(&share_req.manifest_id).ok().flatten())
        .is_some();

    if !is_v1 {
        warn!("libp2p: no custodian record found for manifest_id={}, rejecting share request from {}", share_req.manifest_id, peer);
        let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
        return;
    }

    let handle = tokio::runtime::Handle::current();
    let share_req_clone = share_req.clone();
    let local_peer_id_str = swarm.local_peer_id().to_string();

    let result = {
        let oracle = crate::custodian::oracle_v2::ShamirOracle { local_peer_id: local_peer_id_str };
        std::thread::spawn(move || {
            handle.block_on(crypto::KeyReleaseOracle::release_key_material(&oracle, &share_req_clone))
        })
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("oracle thread panicked")))
    };

    match result {
        Ok(response) => {
            let resp_bytes = response.to_bytes();
            warn!(
                "handle_share_request: sending ShareResponse ({} bytes) for manifest_id={} to peer={} first_bytes={:?}",
                resp_bytes.len(), share_req.manifest_id, peer, &resp_bytes[..resp_bytes.len().min(24)]
            );
            let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, resp_bytes);
            info!(
                "libp2p: released cfrag for manifest_id={} to worker={}",
                share_req.manifest_id, peer
            );
        }
        Err(e) => {
            warn!(
                "libp2p: failed to release share for manifest_id={} to {}: {}",
                share_req.manifest_id, peer, e
            );
            let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
        }
    }
}

/// Handle a `WorkloadDispatch` from the coordinator (Phase 5).
/// If this node is a worker, collect shares from custodians then unseal+deploy.
/// The dispatch contains the sealed_spec (without wrapped_shares) and the custodian peer list.
fn handle_workload_dispatch(
    dispatch: protocol::machine::WorkloadDispatch,
    channel: libp2p::request_response::ResponseChannel<Vec<u8>>,
    peer: libp2p::PeerId,
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
) {
    let manifest_id = dispatch.sealed_spec.manifest_id.clone();

    if crate::podmesh_p2p::is_scheduling_disabled_for(swarm.local_peer_id()) {
        warn!(
            "libp2p: received WorkloadDispatch for {} but we are not a worker",
            manifest_id
        );
        let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, vec![]);
        return;
    }

    info!(
        "libp2p: received WorkloadDispatch for manifest_id={} from coordinator={}",
        manifest_id, peer
    );

    // Ack immediately — share collection + deployment happens asynchronously.
    let _ = swarm.behaviour_mut().scheduler_rr.send_response(channel, b"accepted".to_vec());

    let worker_peer_id = *swarm.local_peer_id();

    // Enqueue the actual work as a control message so it runs in the libp2p task context.
    let local_peer_id_str = worker_peer_id.to_string();
    crate::podmesh_p2p::control::enqueue_control(
        &local_peer_id_str,
        crate::podmesh_p2p::control::Libp2pControl::DeployDispatchedWorkload { dispatch, worker_peer_id },
    );
}

fn load_local_node_cert_bytes() -> Vec<u8> {    if let Ok(home) = std::env::var("HOME") {
        let key_dir = std::path::PathBuf::from(home).join(crypto::KEY_DIR);
        if let Ok(Some(cert)) = protocol::node_cert::load_node_cert(key_dir.to_str().unwrap_or(".podmesh")) {
            return cert.to_bytes();
        }
    }
    vec![]
}

/// Handle scheduler_rr response.
/// Two cases:
/// 1. Response to a CapabilityQuery (sent by initiator) → may contain a CapabilityReply
/// 2. Response to a ResourceQuery (sent after Phase 1) → contains a ResourceReply
fn handle_scheduler_response(
    response: Vec<u8>,
    request_id: libp2p::request_response::OutboundRequestId,
    peer: libp2p::PeerId,
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
    pending_queries: &mut StdHashMap<String, Vec<mpsc::UnboundedSender<String>>>,
) {
    if response.is_empty() {
        // Empty ack — node was not eligible or sent an empty response
        return;
    }

    // Phase 5: check if this is a ShareResponse (raw postcard, no envelope wrapper)
    let local_peer_id = swarm.local_peer_id().to_string();
    if crate::podmesh_p2p::control::deploy_dispatch::try_deliver_share_response(&local_peer_id, &request_id, &response) {
        return;
    }

    // Verify the signed envelope
    let verified = match verify_signed_message(&peer, &response, |err| {
        debug!("libp2p: rejecting scheduler response from {}: {}", peer, err);
    }) {
        Some(e) => e,
        None => return,
    };

    // Case 1: CapabilityReply (response to CapabilityQuery) — trigger Phase 2
    if let Ok(cap_reply) = protocol::machine::root_as_capability_reply(&verified.payload) {
        let query_id = cap_reply.query_id.clone();
        info!(
            "libp2p: received capability reply (inline) for query_id={} from peer={} kem={}",
            query_id, peer, cap_reply.kem_pubkey
        );

        // If this reply is for a seal-and-assign query (role=custodian), forward the
        // candidate to the sealing handler rather than initiating a ResourceQuery.
        if query_id.starts_with("seal:") || query_id.starts_with("custodians:") {
            crate::podmesh_p2p::control::notify_custodian_candidate(
                &query_id,
                crate::custodian::sealer::CustodianCandidate {
                    peer_id: peer.to_string(),
                    kem_pubkey_b64: cap_reply.kem_pubkey.clone(),
                },
            );
            return;
        }

        // Send ResourceQuery to this peer now
        let initiator_pubkey = crypto::ensure_keypair_on_disk()
            .ok()
            .map(|(pk, _)| crypto::b64_encode(&pk))
            .unwrap_or_default();
        let res_query = protocol::machine::build_resource_query(
            &query_id,
            500,
            512 * 1024 * 1024,
            10 * 1024 * 1024 * 1024,
            1,
            &cap_reply.role,
            &initiator_pubkey,
        );
        match utils::sign_payload_default(&res_query, "resource_query", Some("resq")) {
            Ok(signed) => {
                let out_id = swarm.behaviour_mut().scheduler_rr.send_request(&peer, signed);
                crate::podmesh_p2p::control::insert_pending_resource_query(&swarm.local_peer_id().to_string(), out_id, query_id.clone());
                debug!("libp2p: sent resource query id={} to peer={}", query_id, peer);
            }
            Err(e) => {
                warn!("libp2p: failed to sign resource query for id={}: {}", query_id, e);
            }
        }
        return;
    }

    // Case 2: ResourceReply — look up the original query_id and notify observers
    let query_id = match crate::podmesh_p2p::control::take_pending_resource_query(&swarm.local_peer_id().to_string(), &request_id) {
        Some(id) => id,
        None => {
            debug!("libp2p: scheduler response with no pending resource query (possibly stale), ignoring");
            return;
        }
    };

    if let Ok(res_reply) = protocol::machine::root_as_resource_reply(&verified.payload) {
        debug!(
            "libp2p: resource reply ok={} from {} for query_id={}",
            res_reply.ok, peer, query_id
        );
        if res_reply.ok {
            let peer_with_key = format!("{}:{}", peer, res_reply.kem_pubkey);
            utils::notify_capacity_observers(pending_queries, &query_id, move || {
                peer_with_key.clone()
            });
            // If this is a worker-discovery query, also notify the non-blocking dispatch path.
            if query_id.starts_with("worker:") {
                crate::podmesh_p2p::control::dispatch_to_worker::notify_worker_dispatch(
                    &query_id,
                    &peer.to_string(),
                    &res_reply.kem_pubkey,
                );
            }
        } else {
            debug!(
                "libp2p: resource reply not ok for query_id={} reason={}",
                query_id, res_reply.rejection_reason
            );
        }
        return;
    }

    warn!(
        "libp2p: scheduler response: unrecognized payload ({} bytes) from peer={}",
        response.len(),
        peer
    );
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
    owner_pubkey: Option<&str>,
) -> bool {
    let reserve_handle = tokio::runtime::Handle::current();
    let verifier_clone = verifier.clone();
    let request_clone = resource_request.clone();
    let reserve_request_id = request_id.to_string();
    let reserve_manifest_id = manifest_id.to_string();
    let reserve_owner_pubkey = owner_pubkey.map(|s| s.to_string());
    let reservation = std::thread::spawn(move || {
        reserve_handle.block_on(verifier_clone.reserve_capacity(
            &reserve_request_id,
            Some(reserve_manifest_id.as_str()),
            reserve_owner_pubkey.as_deref(),
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

/// Check if this node satisfies a CapabilityQuery based on our node cert.
fn node_satisfies_capability_query_from_cert(cap_q: &protocol::machine::CapabilityQuery) -> bool {
    // Role check
    if !cap_q.role_filter.is_empty() {
        let cert_role = load_local_node_role();
        if let Ok(required) = cap_q.role_filter.parse::<protocol::node_cert::NodeRole>() {
            if !cert_role.has_role_for(&required) {
                return false;
            }
        }
    }
    // Capability check
    if !cap_q.required_capabilities.is_empty() {
        let our_caps = load_local_capabilities();
        let only_default = cap_q.required_capabilities == vec!["default"];
        if !only_default {
            let our_set: std::collections::HashSet<&str> = our_caps.iter().map(|s| s.as_str()).collect();
            for req in &cap_q.required_capabilities {
                if !our_set.contains(req.as_str()) {
                    return false;
                }
            }
        }
    }
    true
}

struct LocalNodeRole(protocol::node_cert::NodeRole);

impl LocalNodeRole {
    fn has_role_for(&self, required: &protocol::node_cert::NodeRole) -> bool {
        use protocol::node_cert::NodeRole;
        match required {
            NodeRole::Worker => matches!(self.0, NodeRole::Worker | NodeRole::Both),
            NodeRole::Custodian => matches!(self.0, NodeRole::Custodian | NodeRole::Both),
            NodeRole::Both => matches!(self.0, NodeRole::Both),
        }
    }
}

fn load_local_node_role() -> LocalNodeRole {
    if let Ok(home) = std::env::var("HOME") {
        let key_dir = std::path::PathBuf::from(home).join(crypto::KEY_DIR);
        if let Ok(Some(cert)) = protocol::node_cert::load_node_cert(key_dir.to_str().unwrap_or(".podmesh")) {
            return LocalNodeRole(cert.role);
        }
    }
    LocalNodeRole(protocol::node_cert::NodeRole::Both)
}

fn load_local_capabilities() -> Vec<String> {
    if let Ok(home) = std::env::var("HOME") {
        let key_dir = std::path::PathBuf::from(home).join(crypto::KEY_DIR);
        if let Ok(Some(cert)) = protocol::node_cert::load_node_cert(key_dir.to_str().unwrap_or(".podmesh")) {
            return cert.capabilities;
        }
    }
    vec!["default".to_string()]
}

fn load_local_node_cert_info() -> (Vec<u8>, Vec<String>, String) {
    if let Ok(home) = std::env::var("HOME") {
        let key_dir = std::path::PathBuf::from(home).join(crypto::KEY_DIR);
        if let Ok(Some(cert)) = protocol::node_cert::load_node_cert(key_dir.to_str().unwrap_or(".podmesh")) {
            let role = cert.role.to_string();
            let caps = cert.capabilities.clone();
            let bytes = cert.to_bytes();
            return (bytes, caps, role);
        }
    }
    (vec![], vec!["default".to_string()], "both".to_string())
}
