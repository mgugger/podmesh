//! Handler for the `SealAndAssignWorkload` control message (Phase 8.8).
//!
//! The scheduler receives a pre-sealed `WorkloadSubmission` from podctl.
//! It does NOT decrypt or generate any key material. Its role is:
//!
//! 1. Validate the submission signature.
//! 2. For each wrapped share in the submission:
//!    a. If the custodian is the local node — store the kfrag directly in `CustodianStore`.
//!    b. If the custodian is a remote peer — send a `WorkloadAssignmentV2` via `scheduler_rr`.
//! 3. If the local node is the elected coordinator, enqueue `DispatchToWorker`.
//! 4. Reply to the REST handler with the list of custodian peer IDs.

use libp2p::{Swarm, gossipsub};
use log::{info, warn};
use std::collections::HashMap as StdHashMap;
use tokio::sync::{mpsc, oneshot};

use protocol::machine::{WorkloadAssignmentV2, WorkloadSubmission};
use crate::podmesh_p2p::behaviour::MyBehaviour;
use crate::podmesh_p2p::utils;

/// Distribute a pre-sealed `WorkloadSubmission` to custodians.
/// Called from within the libp2p swarm task.
pub async fn handle_seal_and_assign(
    submission: WorkloadSubmission,
    reply_tx: oneshot::Sender<Result<Vec<String>, String>>,
    swarm: &mut Swarm<MyBehaviour>,
    _topic: &gossipsub::IdentTopic,
    _pending_queries: &mut StdHashMap<String, Vec<mpsc::UnboundedSender<String>>>,
) {
    let manifest_id = submission.sealed_spec.manifest_id.clone();

    // --- Validate submission signature ---
    if let Err(e) = submission.verify_submission_sig() {
        let msg = format!("WorkloadSubmission signature invalid for {}: {}", manifest_id, e);
        warn!("{}", msg);
        let _ = reply_tx.send(Err(msg));
        return;
    }

    info!(
        "seal_and_assign: manifest_id={} accepted (kfrag_count={}, threshold={}, shares={})",
        manifest_id,
        submission.sealed_spec.kfrag_count,
        submission.sealed_spec.kfrag_threshold,
        submission.wrapped_shares.len(),
    );

    let local_peer_id = swarm.local_peer_id().to_string();

    // Load the local signing pubkey once — it is embedded in every WorkloadAssignmentV2
    // so remote custodians can later verify worker assignment tokens.
    let coordinator_pubkey_b64 = crypto::ensure_keypair_on_disk()
        .ok()
        .map(|(pk, _)| crypto::b64_encode(&pk))
        .unwrap_or_default();

    let all_custodian_peers: Vec<String> = submission
        .wrapped_shares
        .iter()
        .map(|s| s.custodian_peer_id.clone())
        .collect();

    let mut assigned_peers: Vec<String> = Vec::new();

    for share in &submission.wrapped_shares {
        let peer_id_str = &share.custodian_peer_id;

        if peer_id_str == &local_peer_id {
            // --- Local custodian: store directly ---
            match store_local_kfrag(
                &manifest_id,
                &submission,
                share.share_index,
                &share.wrapped_bytes,
                &all_custodian_peers,
                &coordinator_pubkey_b64,
                &local_peer_id,
            ) {
                Ok(()) => {
                    info!(
                        "seal_and_assign: stored local kfrag for manifest_id={} index={}",
                        manifest_id, share.share_index
                    );
                    assigned_peers.push(peer_id_str.clone());
                }
                Err(e) => {
                    warn!(
                        "seal_and_assign: failed to store local kfrag for manifest_id={}: {}",
                        manifest_id, e
                    );
                }
            }
        } else {
            // --- Remote custodian: send WorkloadAssignmentV2 ---
            let peer_id = match peer_id_str.parse::<libp2p::PeerId>() {
                Ok(id) => id,
                Err(e) => {
                    warn!(
                        "seal_and_assign: invalid peer_id '{}' for manifest_id={}: {}",
                        peer_id_str, manifest_id, e
                    );
                    continue;
                }
            };

            let scheduler_sig = build_scheduler_sig(&submission.sealed_spec.to_bytes());

            let assignment = WorkloadAssignmentV2 {
                sealed_spec: submission.sealed_spec.clone(),
                all_custodian_peers: all_custodian_peers.clone(),
                required_capabilities: submission.required_capabilities.clone(),
                scheduler_sig,
                wrapped_kfrag: share.wrapped_bytes.clone(),
                kfrag_index: share.share_index,
                coordinator_pubkey: coordinator_pubkey_b64.clone(),
            };

            let assignment_bytes = assignment.to_bytes();

            // Wrap in signed envelope so the receiver can verify integrity.
            let signed_assignment = match utils::sign_payload_default(&assignment_bytes, "workload_assignment_v2", Some("wassign")) {
                Ok(signed) => signed,
                Err(e) => {
                    warn!(
                        "seal_and_assign: failed to sign WorkloadAssignmentV2 for peer={}: {}",
                        peer_id_str, e
                    );
                    continue;
                }
            };

            swarm
                .behaviour_mut()
                .scheduler_rr
                .send_request(&peer_id, signed_assignment);

            info!(
                "seal_and_assign: sent WorkloadAssignmentV2 for manifest_id={} to peer={}",
                manifest_id, peer_id
            );
            assigned_peers.push(peer_id_str.clone());
        }
    }

    // If the local node is the elected coordinator, trigger worker dispatch.
    if crate::custodian::coordinator::is_coordinator(
        &manifest_id,
        &local_peer_id,
        &all_custodian_peers,
    ) {
        info!(
            "seal_and_assign: local node is coordinator for manifest_id={}, enqueuing DispatchToWorker",
            manifest_id
        );
        crate::podmesh_p2p::control::enqueue_control(
            &local_peer_id.to_string(),
            crate::podmesh_p2p::control::Libp2pControl::DispatchToWorker {
                sealed_spec: submission.sealed_spec.clone(),
                all_custodian_peers: all_custodian_peers.clone(),
                required_capabilities: submission.required_capabilities.clone(),
                replica_count: submission.replica_count,
            },
        );
    }

    let _ = reply_tx.send(Ok(assigned_peers));
}

fn store_local_kfrag(
    manifest_id: &str,
    submission: &WorkloadSubmission,
    kfrag_index: u8,
    wrapped_bytes: &[u8],
    all_custodian_peers: &[String],
    coordinator_pubkey: &str,
    local_peer_id: &str,
) -> anyhow::Result<()> {
    let store = crate::storage::get_custodian_store()
        .ok_or_else(|| anyhow::anyhow!("custodian store not initialized"))?;

    let spec = &submission.sealed_spec;
    let record = crate::storage::CustodianRecord::new(
        manifest_id.to_string(),
        spec.owner_pubkey.clone(),
        spec.kfrag_count,
        spec.kfrag_threshold,
        kfrag_index,
        wrapped_bytes.to_vec(),
        all_custodian_peers.to_vec(),
    ).with_coordinator_pubkey(coordinator_pubkey.to_string())
     .with_local_peer_id(local_peer_id.to_string());

    store.set_record(&record)?;
    Ok(())
}

fn build_scheduler_sig(sealed_bytes: &[u8]) -> String {
    crypto::ensure_keypair_on_disk()
        .ok()
        .and_then(|(_, sk)| crypto::sign_data_with_key(&sk, sealed_bytes).ok())
        .map(|s| crypto::b64_encode(&s))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Custodian candidate channel — used by GET /api/v1/custodians
// ---------------------------------------------------------------------------

use once_cell::sync::Lazy;
use std::sync::Mutex;

static CUSTODIAN_REPLY_MAP: Lazy<Mutex<StdHashMap<String, mpsc::UnboundedSender<crate::custodian::sealer::CustodianCandidate>>>> =
    Lazy::new(|| Mutex::new(StdHashMap::new()));

pub fn register_custodian_reply_channel(query_id: &str, tx: mpsc::UnboundedSender<crate::custodian::sealer::CustodianCandidate>) {
    CUSTODIAN_REPLY_MAP
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(query_id.to_string(), tx);
}

pub fn remove_custodian_reply_channel(query_id: &str) {
    CUSTODIAN_REPLY_MAP
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(query_id);
}

/// Called by the scheduler_message handler when a CapabilityReply arrives for a `custodians:`
/// query. Forwards the candidate to the waiting REST handler.
pub fn notify_custodian_candidate(query_id: &str, candidate: crate::custodian::sealer::CustodianCandidate) {
    let map = CUSTODIAN_REPLY_MAP
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some(tx) = map.get(query_id) {
        let _ = tx.send(candidate);
    }
}
