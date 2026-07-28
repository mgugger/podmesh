use std::{
    collections::{HashSet, VecDeque},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use iroh::{
    Endpoint, EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
};
use iroh_gossip::{
    api::{Event, GossipSender},
    net::Gossip,
    proto::TopicId,
};
use protocol::{CapacityQuery, MAX_CAPACITY_MESSAGE_BYTES};
use tokio::{sync::broadcast, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::ValidatedMachineConfig;
use super::{AgentAttachmentHandler, CapacityOfferHandler, MemberRegistry, PlacementHandler};

pub const SCHEDULER_GOSSIP_ALPN: &[u8] = b"/podmesh/scheduler-gossip/1";
pub const SCHEDULER_GOSSIP_TOPIC: TopicId = TopicId::from_bytes([0x50; 32]);

#[derive(Debug, Clone)]
struct AuthorizedGossip {
    gossip: Gossip,
    allowed_members: MemberRegistry,
}

impl ProtocolHandler for AuthorizedGossip {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if !self.allowed_members.contains(&connection.remote_id()) {
            connection.close(403u16.into(), b"scheduler membership required");
            return Err(AcceptError::from_err(std::io::Error::other(
                "scheduler membership required",
            )));
        }
        self.gossip
            .handle_connection(connection)
            .await
            .map_err(AcceptError::from_err)
    }

    async fn shutdown(&self) {
        if let Err(error) = self.gossip.shutdown().await {
            log::warn!("scheduler gossip shutdown failed: {error}");
        }
    }
}

pub struct SchedulerGossip {
    sender: GossipSender,
    query_events: broadcast::Sender<CapacityQuery>,
    router: Router,
    receiver_task: JoinHandle<Result<()>>,
    cancellation: CancellationToken,
    members: MemberRegistry,
}

#[derive(Clone)]
pub struct GossipPublisher {
    sender: GossipSender,
    query_events: broadcast::Sender<CapacityQuery>,
}

/// Handle for dialing scheduler peers that were discovered after startup.
#[derive(Clone)]
pub struct PeerJoiner {
    sender: GossipSender,
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
        let bytes = query.to_bytes(now_secs())?;
        let _ = self.query_events.send(query);
        self.sender
            .broadcast(bytes.into())
            .await
            .context("broadcast capacity query")
    }
}

impl SchedulerGossip {
    pub async fn start(
        endpoint: Endpoint,
        config: &ValidatedMachineConfig,
        attachment_handler: AgentAttachmentHandler,
        offer_handler: CapacityOfferHandler,
        placement_handler: PlacementHandler,
    ) -> Result<Self> {
        let gossip = Gossip::builder()
            .alpn(SCHEDULER_GOSSIP_ALPN)
            .max_message_size(MAX_CAPACITY_MESSAGE_BYTES)
            .spawn(endpoint.clone());
        ensure!(
            gossip.max_message_size() == MAX_CAPACITY_MESSAGE_BYTES,
            "scheduler gossip message bound was not applied"
        );
        let members = MemberRegistry::new(config.scheduler_members.clone())?;
        let router = Router::builder(endpoint)
            .accept(
                SCHEDULER_GOSSIP_ALPN,
                AuthorizedGossip {
                    gossip: gossip.clone(),
                    allowed_members: members.clone(),
                },
            )
            .accept(protocol::AGENT_CAPACITY_ALPN, attachment_handler)
            .accept(protocol::CAPACITY_OFFER_ALPN, offer_handler)
            .accept(protocol::SCHEDULER_PLACEMENT_ALPN, placement_handler)
            .spawn();
        // Subscription must not depend on a peer being up. A scheduler that
        // cannot reach its bootstrap peers still serves clients and attached
        // agents; discovery keeps trying to join the mesh in the background,
        // so a set of schedulers started together converges instead of all of
        // them failing on each other.
        let topic = gossip
            .subscribe(SCHEDULER_GOSSIP_TOPIC, Vec::new())
            .await
            .context("subscribe to scheduler gossip topic")?;
        let (sender, mut receiver) = topic.split();
        if !config.scheduler_bootstraps.is_empty() {
            sender
                .join_peers(config.scheduler_bootstraps.clone())
                .await
                .context("dial configured scheduler bootstrap peers")?;
            // Waiting for a neighbour keeps a normal start deterministic, but
            // timing out is not fatal. Peers that are still coming up are
            // dialled again by background discovery, so a mesh started all at
            // once converges instead of every member failing on every other.
            match tokio::time::timeout(config.query_timeout, receiver.joined()).await {
                Ok(Ok(())) => log::info!(
                    "joined scheduler gossip mesh via {} bootstrap peers",
                    config.scheduler_bootstraps.len()
                ),
                Ok(Err(error)) => {
                    log::warn!("joining scheduler gossip mesh failed, will retry: {error}")
                }
                Err(_) => log::warn!("joining scheduler gossip mesh timed out, will retry"),
            }
        }
        let event_capacity = config.max_pending_queries.min(config.max_seen_queries);
        let (query_events, _) = broadcast::channel(event_capacity);
        let event_tx = query_events.clone();
        let cancellation = CancellationToken::new();
        let receiver_cancellation = cancellation.clone();
        let receiver_members = members.clone();
        let max_seen = config.max_seen_queries;
        let receiver_task = tokio::spawn(async move {
            let mut seen = SeenQueries::new(max_seen);
            loop {
                tokio::select! {
                    _ = receiver_cancellation.cancelled() => return Ok(()),
                    event = receiver.next() => match event {
                        Some(Ok(Event::Received(message))) => {
                            match validate_received_query(&message.content, &receiver_members, &mut seen) {
                                Ok(Some(query)) => {
                                    let _ = event_tx.send(query);
                                }
                                Ok(None) => {}
                                Err(error) => log::warn!("scheduler gossip query rejected: {error}"),
                            }
                        }
                        Some(Ok(Event::Lagged)) => {
                            log::warn!("scheduler gossip receiver lagged; messages were dropped");
                        }
                        Some(Ok(Event::NeighborUp(endpoint_id))) => {
                            log::debug!("scheduler gossip neighbor connected: {}", endpoint_id.fmt_short());
                        }
                        Some(Ok(Event::NeighborDown(endpoint_id))) => {
                            log::debug!("scheduler gossip neighbor disconnected: {}", endpoint_id.fmt_short());
                        }
                        Some(Err(error)) => return Err(anyhow::anyhow!("scheduler gossip receive failed: {error}")),
                        None => return Err(anyhow::anyhow!("scheduler gossip subscription closed")),
                    }
                }
            }
        });
        Ok(Self {
            sender,
            query_events,
            router,
            receiver_task,
            cancellation,
            members,
        })
    }

    /// Handle onto the converging member allowlist, so peer discovery can
    /// admit schedulers that were not reachable at startup.
    pub fn members(&self) -> MemberRegistry {
        self.members.clone()
    }

    /// Dials newly discovered peers into the gossip mesh.
    pub async fn join_peers(&self, peers: Vec<EndpointId>) -> Result<()> {
        self.peer_joiner().join_peers(peers).await
    }

    /// Cloneable handle for dialing peers discovered after startup.
    pub fn peer_joiner(&self) -> PeerJoiner {
        PeerJoiner {
            sender: self.sender.clone(),
        }
    }

    pub fn subscribe_queries(&self) -> broadcast::Receiver<CapacityQuery> {
        self.query_events.subscribe()
    }

    pub fn publisher(&self) -> GossipPublisher {
        GossipPublisher {
            sender: self.sender.clone(),
            query_events: self.query_events.clone(),
        }
    }

    pub async fn publish(&self, query: CapacityQuery) -> Result<()> {
        self.publisher().publish(query).await
    }

    pub async fn join(&mut self) -> Result<Result<()>, tokio::task::JoinError> {
        (&mut self.receiver_task).await
    }

    pub async fn shutdown(self) -> Result<()> {
        self.cancellation.cancel();
        self.router.shutdown().await?;
        self.receiver_task
            .await
            .context("join scheduler gossip receiver task")??;
        Ok(())
    }
}

#[derive(Debug)]
struct SeenQueries {
    order: VecDeque<(Vec<u8>, String)>,
    entries: HashSet<(Vec<u8>, String)>,
    capacity: usize,
}

impl SeenQueries {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            entries: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    fn insert(&mut self, key: (Vec<u8>, String)) -> bool {
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
    bytes: &[u8],
    members: &MemberRegistry,
    seen: &mut SeenQueries,
) -> Result<Option<CapacityQuery>> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_CAPACITY_MESSAGE_BYTES,
        "capacity gossip message size is invalid"
    );
    let query = CapacityQuery::from_bytes(bytes, now_secs())?;
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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
