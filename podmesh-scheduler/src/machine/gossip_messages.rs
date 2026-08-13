//! Validation and handling of individual scheduler gossip messages.
//!
//! Kept apart from the gossip transport so the socket-level plumbing and the
//! rules about which messages are trustworthy can be read independently.

use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;
use protocol::CapacityQuery;
use tokio::sync::OnceCell;

use super::MemberRegistry;

#[derive(Debug)]
pub(super) struct SeenQueries {
    order: VecDeque<(Vec<u8>, String)>,
    entries: HashSet<(Vec<u8>, String)>,
    capacity: usize,
}

impl SeenQueries {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            entries: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    pub(super) fn insert(&mut self, key: (Vec<u8>, String)) -> bool {
        if self.entries.contains(&key) {
            return false;
        }
        if self.entries.len() == self.capacity
            && let Some(expired) = self.order.pop_front()
        {
            self.entries.remove(&expired);
        }
        self.order.push_back(key.clone());
        self.entries.insert(key)
    }
}

fn validate_received_query(
    query: CapacityQuery,
    members: &MemberRegistry,
    seen: &mut SeenQueries,
) -> Result<Option<CapacityQuery>> {
    query.verify(crate::now_secs())?;
    let endpoint_bytes: [u8; 32] = query
        .reply_endpoint
        .endpoint_id
        .as_slice()
        .try_into()
        .context("capacity query reply EndpointId length is invalid")?;
    let endpoint_id = EndpointId::from_bytes(&endpoint_bytes)
        .context("capacity query reply EndpointId is invalid")?;
    ensure!(
        members.contains(&endpoint_id),
        "capacity query reply endpoint is not an authorized scheduler"
    );
    let key = (
        query.reply_endpoint.endpoint_id.clone(),
        query.query_id.clone(),
    );
    Ok(seen.insert(key).then_some(query))
}

/// A gossip message this scheduler should act on.
pub(super) enum ReceivedGossip {
    Capacity(Box<CapacityQuery>),
    Locate(Box<protocol::AgentLocationQuery>),
    Announcement(Box<protocol::EndpointRecord>),
}

/// Validate a gossip message and say what it is.
pub(super) fn classify(
    bytes: &[u8],
    members: &MemberRegistry,
    seen: &mut SeenQueries,
) -> Result<Option<ReceivedGossip>> {
    match protocol::SchedulerGossipMessage::from_bytes(bytes)? {
        protocol::SchedulerGossipMessage::Capacity(query) => {
            Ok(validate_received_query(*query, members, seen)?
                .map(|query| ReceivedGossip::Capacity(Box::new(query))))
        }
        protocol::SchedulerGossipMessage::Locate(query) => {
            query.verify(crate::now_secs())?;
            let asker = decode_endpoint(&query.reply_endpoint.endpoint_id)?;
            ensure!(
                members.contains(&asker),
                "agent location query came from an unauthorized scheduler"
            );
            if !seen.insert((asker.as_bytes().to_vec(), query.query_id.clone())) {
                return Ok(None);
            }
            Ok(Some(ReceivedGossip::Locate(query)))
        }
        protocol::SchedulerGossipMessage::Announcement(record) => {
            record.verify(crate::now_secs())?;
            Ok(Some(ReceivedGossip::Announcement(record)))
        }
    }
}

/// Answer a location query, but only if this scheduler holds the agent.
pub(super) async fn answer_location(
    responder: super::LocationResponder,
    forwarder: Arc<OnceCell<super::AgentControlForwarder>>,
    query: protocol::AgentLocationQuery,
) {
    let Some(forwarder) = forwarder.get() else {
        return;
    };
    let Ok(agent) = decode_endpoint(&query.agent_endpoint_id) else {
        return;
    };
    if !forwarder.holds_attachment(agent).await {
        return;
    }
    if let Err(error) = responder.answer(&query).await {
        log::debug!(
            "answering location query for agent {} failed: {error:#}",
            agent.fmt_short()
        );
    }
}

/// Admit a scheduler that announced itself on the mesh.
///
/// Membership learned this way is transitive: a peer can only announce once it
/// is already in the mesh, which means an operator-configured scheduler admitted
/// it. That is what lets a mesh grow past the peers written down on each node.
/// The announcement's signing key is bound to its endpoint id, which lets that
/// scheduler authorise *itself* onto this scheduler's machine relay. Unrestricted
/// relay-issuer trust is deliberately *not* granted here — that stays with
/// explicitly pinned peers, so a transitively admitted scheduler cannot mint
/// relay grants naming anybody else.
pub(super) fn admit_announced_peer(
    members: &MemberRegistry,
    member_issuers: &super::MemberIssuers,
    lookup: &iroh::address_lookup::memory::MemoryLookup,
    record: protocol::EndpointRecord,
) {
    let Ok(endpoint_id) = decode_endpoint(&record.endpoint_id) else {
        return;
    };
    let Ok(address) = iroh_support::endpoint_addr(&record, crate::now_secs()) else {
        return;
    };
    lookup.set_endpoint_info(address);
    member_issuers.bind(&record.signing_pubkey, record.endpoint_id.clone());
    if members.insert(endpoint_id) {
        log::info!(
            "admitted announced scheduler {} into the gossip mesh",
            endpoint_id.fmt_short()
        );
    }
}

fn decode_endpoint(bytes: &[u8]) -> Result<EndpointId> {
    let fixed: [u8; 32] = bytes
        .try_into()
        .context("scheduler gossip EndpointId length is invalid")?;
    EndpointId::from_bytes(&fixed).context("scheduler gossip EndpointId is invalid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seen_query_cache_deduplicates_and_evicts_with_a_fixed_bound() {
        let mut seen = SeenQueries::new(2);
        assert!(seen.insert((vec![1], "one".into())));
        assert!(!seen.insert((vec![1], "one".into())));
        assert!(seen.insert((vec![2], "two".into())));
        assert!(seen.insert((vec![3], "three".into())));
        assert_eq!(seen.entries.len(), 2);
        assert!(seen.insert((vec![1], "one".into())));
        assert_eq!(seen.entries.len(), 2);
    }
}
