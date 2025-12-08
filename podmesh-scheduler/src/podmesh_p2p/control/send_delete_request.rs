use libp2p::{PeerId, Swarm};
use log::{info, warn};
use tokio::sync::mpsc;

use crate::podmesh_p2p::behaviour::MyBehaviour;

/// Handle sending a delete request to a specific peer via libp2p request-response
pub async fn handle_send_delete_request(
    peer_id: PeerId,
    delete_request: Vec<u8>,
    reply_tx: mpsc::UnboundedSender<Result<String, String>>,
    swarm: &mut Swarm<MyBehaviour>,
) {
    info!(
        "handle_send_delete_request: sending delete request to peer {}",
        peer_id
    );

    // Handle self-delete locally to avoid dialing ourselves through RequestResponse
    if peer_id == *swarm.local_peer_id() {
        info!(
            "handle_send_delete_request: processing self-delete request for peer {}",
            peer_id
        );

        match crate::workload_integration::process_enhanced_self_delete_request(&delete_request)
            .await
        {
            Ok(_) => {
                let _ = reply_tx.send(Ok("Delete request handled locally".into()));
            }
            Err(e) => {
                warn!(
                    "handle_send_delete_request: self-delete processing failed for peer {}: {}",
                    peer_id, e
                );
                let _ = reply_tx.send(Err(format!(
                    "Self-delete processing failed: {}",
                    e
                )));
            }
        }

        return;
    }

    // The delete request should already be signed by the CLI when coming through the REST API
    // Just forward it as-is to preserve the original signature for verification on the worker node
    let request_id = swarm
        .behaviour_mut()
        .delete_rr
        .send_request(&peer_id, delete_request);

    info!(
        "handle_send_delete_request: delete request sent to peer {} with request_id {:?}",
        peer_id, request_id
    );

    // For now, send immediate success response
    // In a complete implementation, we would track the request_id and wait for the actual response
    // from the peer, but that requires more complex request tracking infrastructure
    let _ = reply_tx.send(Ok("Delete request sent".into()));
}
