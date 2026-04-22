//! Handler for `DiscoverCustodians` control message.
//!
//! Broadcasts a `CapabilityQuery(role_filter="custodian")` and collects
//! `CapabilityReply` responses until `max` candidates are found or timeout expires.
//! Results (peer_id + kem_pubkey_b64) are returned to the caller via oneshot.
//!
//! The collection loop runs in a **separate spawned task** so the swarm event loop
//! is not blocked and can process incoming `scheduler_rr` response events.

use libp2p::{Swarm, gossipsub};
use log::{debug, info, warn};
use tokio::sync::oneshot;

use protocol::machine::{CustodianInfo, build_capability_query};
use crate::custodian::sealer::CustodianCandidate;
use crate::podmesh_p2p::behaviour::MyBehaviour;
use crate::podmesh_p2p::utils;
use super::seal_and_assign::{register_custodian_reply_channel, remove_custodian_reply_channel};

const DISCOVERY_TIMEOUT_MS: u64 = 5_000;

pub async fn handle_discover_custodians(
    max: usize,
    reply_tx: oneshot::Sender<Vec<CustodianInfo>>,
    swarm: &mut Swarm<MyBehaviour>,
    _topic: &gossipsub::IdentTopic,
) {
    let query_id = format!("custodians:{}", uuid::Uuid::new_v4());
    let nonce = utils::make_nonce(Some("disc"));
    let initiator_pubkey = crypto::ensure_keypair_on_disk()
        .ok()
        .map(|(pk, _)| crypto::b64_encode(&pk))
        .unwrap_or_default();

    let cap_query_bytes = build_capability_query(
        &query_id,
        &nonce,
        &[],
        "custodian",
        &initiator_pubkey,
    );

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<CustodianCandidate>();
    register_custodian_reply_channel(&query_id, tx);

    // Include self if this node is a custodian — inject directly into channel.
    if crate::podmesh_p2p::get_node_mode().is_custodian() {
        let local_peer = swarm.local_peer_id().to_string();
        if let Ok((kem_pub, _)) = crypto::ensure_kem_keypair_on_disk() {
            super::seal_and_assign::notify_custodian_candidate(
                &query_id,
                CustodianCandidate {
                    peer_id: local_peer.clone(),
                    kem_pubkey_b64: crypto::b64_encode(&kem_pub),
                },
            );
            debug!("discover_custodians: local node {} is a custodian", local_peer);
        }
    }

    match utils::broadcast_signed_request_to_peers(swarm, &cap_query_bytes, "capability_query") {
        Ok(sent) => info!("discover_custodians: broadcast {} to {} peers", query_id, sent),
        Err(e) => warn!("discover_custodians: broadcast failed: {:?}", e),
    }

    // Spawn the collection loop in a separate task so the swarm event loop is NOT
    // blocked and can process incoming scheduler_rr response events (which deliver
    // CapabilityReplies via notify_custodian_candidate → the channel above).
    let query_id_clone = query_id.clone();
    tokio::spawn(async move {
        let mut rx = rx;
        let mut custodians: Vec<CustodianInfo> = Vec::new();
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(DISCOVERY_TIMEOUT_MS);

        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(c)) => {
                    if !custodians.iter().any(|x| x.peer_id == c.peer_id) {
                        custodians.push(CustodianInfo {
                            peer_id: c.peer_id,
                            kem_pubkey_b64: c.kem_pubkey_b64,
                        });
                        if custodians.len() >= max {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }

        remove_custodian_reply_channel(&query_id_clone);
        info!("discover_custodians: found {} custodians", custodians.len());
        let _ = reply_tx.send(custodians);
    });
}
