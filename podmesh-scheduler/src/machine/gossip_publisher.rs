//! Publishing side of the scheduler gossip topic.
//!
//! Broadcasting is separated from receiving because the two have different
//! lifetimes: a publisher is a cheap clone handed to background tasks such as
//! peer discovery and the control relay, long after the receiver loop started.

use anyhow::{Context, Result};
use iroh::EndpointId;
use iroh_gossip::api::GossipSender;
use protocol::CapacityQuery;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct GossipPublisher {
    pub(super) sender: GossipSender,
    pub(super) query_events: broadcast::Sender<CapacityQuery>,
}

/// Handle for dialing scheduler peers that were discovered after startup.
#[derive(Clone)]
pub struct PeerJoiner {
    pub(super) sender: GossipSender,
}

impl PeerJoiner {
    pub async fn join_peers(&self, peers: Vec<EndpointId>) -> Result<()> {
        self.sender
            .join_peers(peers)
            .await
            .context("join discovered scheduler peers")
    }
}

impl GossipPublisher {
    pub async fn publish(&self, query: CapacityQuery) -> Result<()> {
        // Verified on the way out, so a malformed query is caught here rather
        // than by every receiver.
        query.verify(crate::now_secs())?;
        let bytes =
            protocol::SchedulerGossipMessage::Capacity(Box::new(query.clone())).to_bytes()?;
        let _ = self.query_events.send(query);
        self.sender
            .broadcast(bytes.into())
            .await
            .context("broadcast capacity query")
    }

    /// Ask the mesh which scheduler holds an agent's attachment.
    pub async fn publish_location(&self, query: protocol::AgentLocationQuery) -> Result<()> {
        let bytes = protocol::SchedulerGossipMessage::Locate(Box::new(query)).to_bytes()?;
        self.sender
            .broadcast(bytes.into())
            .await
            .context("broadcast agent location query")
    }

    pub async fn publish_reconciliation(
        &self,
        query: protocol::SchedulerReconciliationQuery,
    ) -> Result<()> {
        query.verify(crate::now_secs())?;
        let bytes = protocol::SchedulerGossipMessage::Reconcile(Box::new(query)).to_bytes()?;
        self.sender
            .broadcast(bytes.into())
            .await
            .context("broadcast reconciliation query")
    }

    /// Announce this scheduler so peers beyond the configured bootstrap URLs
    /// learn how to reach it.
    pub async fn publish_announcement(&self, record: protocol::EndpointRecord) -> Result<()> {
        let bytes = protocol::SchedulerGossipMessage::Announcement(Box::new(record)).to_bytes()?;
        self.sender
            .broadcast(bytes.into())
            .await
            .context("broadcast scheduler announcement")
    }
}
