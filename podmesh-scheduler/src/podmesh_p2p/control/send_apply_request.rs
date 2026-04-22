use libp2p::{PeerId, Swarm};
use log::{debug, info};
use tokio::sync::mpsc;

use crate::podmesh_p2p::behaviour::MyBehaviour;
use crate::podmesh_p2p::control::{insert_pending_apply_response};

/// Handle SendApplyRequest control message
pub async fn handle_send_apply_request(
    peer_id: PeerId,
    manifest: Vec<u8>,
    reply_tx: mpsc::UnboundedSender<Result<String, String>>,
    swarm: &mut Swarm<MyBehaviour>,
) {
    info!(
        "libp2p: control SendApplyRequest received for peer={}",
        peer_id
    );

    // Check if this is a self-send - handle locally instead of using RequestResponse
    if peer_id == *swarm.local_peer_id() {
        debug!("libp2p: handling self-apply locally for peer {}", peer_id);

        // Use the new workload manager integration for self-apply as well
        crate::workload_integration::process_enhanced_self_apply_request(&manifest, swarm).await;

        let _ = reply_tx.send(Ok(format!("Apply request handled locally for {}", peer_id)));
        return;
    }

    // For remote peers, use the normal RequestResponse protocol
    // The apply request should already be signed by the owner (CLI/podctl) when coming through apply_direct
    // Forward as-is to preserve the original owner's signature - never re-sign
    let request_id = swarm
        .behaviour_mut()
        .apply_rr
        .send_request(&peer_id, manifest);
    info!(
        "libp2p: sent apply request to peer={} request_id={:?}",
        peer_id, request_id
    );

    // Store the reply channel keyed by the OutboundRequestId so we can resolve it
    // when the remote peer sends back an ApplyResponse.
    let key = format!("{}:{:?}", swarm.local_peer_id(), request_id);
    insert_pending_apply_response(key, reply_tx);
}
