use libp2p::{gossipsub, PeerId, Swarm};
use std::collections::HashMap as StdHashMap;
use tokio::sync::mpsc;

use crate::libp2p_beemesh::{behaviour::MyBehaviour};

mod query_capacity;
mod send_apply_request;

pub use query_capacity::handle_query_capacity_with_payload;
pub use send_apply_request::handle_send_apply_request;

/// Handle incoming control messages from other parts of the host (REST handlers)
pub async fn handle_control_message(
    msg: Libp2pControl,
    swarm: &mut Swarm<MyBehaviour>,
    topic: &gossipsub::IdentTopic,
    pending_queries: &mut StdHashMap<String, Vec<mpsc::UnboundedSender<String>>>,
) {
    match msg {
        Libp2pControl::QueryCapacityWithPayload { request_id, reply_tx, payload } => {
            handle_query_capacity_with_payload(request_id, reply_tx, payload, swarm, topic, pending_queries).await;
        }
        Libp2pControl::SendApplyRequest { peer_id, manifest, reply_tx } => {
            handle_send_apply_request(peer_id, manifest, reply_tx, swarm).await;
        }
    }
}

/// Control messages sent from the rest API or other parts of the host to the libp2p task.
#[derive(Debug)]
pub enum Libp2pControl {
    QueryCapacityWithPayload {
        request_id: String,
        reply_tx: mpsc::UnboundedSender<String>,
        payload: Vec<u8>,
    },
    SendApplyRequest {
        peer_id: PeerId,
        manifest: serde_json::Value,
        reply_tx: mpsc::UnboundedSender<Result<String, String>>,
    },
}
