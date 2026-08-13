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
use super::gossip_messages::{ReceivedGossip, SeenQueries, admit_announced_peer, answer_location};
use super::gossip_publisher::{GossipPublisher, PeerJoiner};
use super::{
    AgentAttachmentHandler, AgentControlRelayHandler, CapacityOfferHandler, MemberRegistry,
    PlacementHandler,
};

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
    control_relay: AgentControlRelayHandler,
}

/// Everything the gossip runtime serves on, or shares with, the rest of the
/// scheduler.
///
/// Grouped rather than passed one by one because these are all handles into the
/// same running scheduler, and a long positional list invites mismatching two of
/// them at a call site.
pub struct SchedulerGossipServices {
    pub endpoint: Endpoint,
    pub attachments: AgentAttachmentHandler,
    pub offers: CapacityOfferHandler,
    pub placement: PlacementHandler,
    /// Shared with the control relay, which resolves the queries answered here.
    pub locations: super::LocationRegistry,
    /// Extended as peers announce themselves, so the machine relay can admit
    /// members it was never pinned to.
    pub member_issuers: super::MemberIssuers,
    pub lookup: iroh::address_lookup::memory::MemoryLookup,
}

impl SchedulerGossip {
    pub async fn start(
        services: SchedulerGossipServices,
        config: &ValidatedMachineConfig,
    ) -> Result<Self> {
        let SchedulerGossipServices {
            endpoint,
            attachments: attachment_handler,
            offers: offer_handler,
            placement: placement_handler,
            locations,
            member_issuers,
            lookup,
        } = services;
        let responder_endpoint = endpoint.clone();
        let gossip = Gossip::builder()
            .alpn(SCHEDULER_GOSSIP_ALPN)
            .max_message_size(MAX_CAPACITY_MESSAGE_BYTES)
            .spawn(endpoint.clone());
        ensure!(
            gossip.max_message_size() == MAX_CAPACITY_MESSAGE_BYTES,
            "scheduler gossip message bound was not applied"
        );
        let members = MemberRegistry::new(config.scheduler_members.clone())?;
        // The forwarder cannot exist before the router is up, so the handler is
        // created here and completed with `install` once startup finishes.
        let control_relay = AgentControlRelayHandler::new(
            members.clone(),
            locations.clone(),
            crate::clientapi::MAX_CONCURRENT_CLIENT_RELAYS,
            crate::clientapi::CLIENT_RELAY_TIMEOUT,
        );
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
            .accept(protocol::AGENT_CONTROL_RELAY_ALPN, control_relay.clone())
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
        let receiver_lookup = lookup.clone();
        let receiver_issuers = member_issuers.clone();
        let receiver_responder = super::LocationResponder::new(
            responder_endpoint,
            crate::clientapi::CLIENT_RELAY_TIMEOUT,
        );
        let receiver_forwarder = control_relay.forwarder_handle();
        let max_seen = config.max_seen_queries;
        let receiver_task = tokio::spawn(async move {
            let mut seen = SeenQueries::new(max_seen);
            loop {
                tokio::select! {
                    _ = receiver_cancellation.cancelled() => return Ok(()),
                    event = receiver.next() => match event {
                        Some(Ok(Event::Received(message))) => {
                            match super::gossip_messages::classify(
                                &message.content,
                                &receiver_members,
                                &mut seen,
                            ) {
                                Ok(Some(ReceivedGossip::Capacity(query))) => {
                                    let _ = event_tx.send(*query);
                                }
                                Ok(Some(ReceivedGossip::Locate(query))) => {
                                    // Only the scheduler that holds the agent
                                    // answers, so this costs the rest one
                                    // membership check.
                                    let responder = receiver_responder.clone();
                                    let forwarder = receiver_forwarder.clone();
                                    tokio::spawn(async move {
                                        answer_location(responder, forwarder, *query).await;
                                    });
                                }
                                Ok(Some(ReceivedGossip::Announcement(record))) => {
                                    admit_announced_peer(
                                        &receiver_members,
                                        &receiver_issuers,
                                        &receiver_lookup,
                                        *record,
                                    );
                                }
                                Ok(None) => {}
                                Err(error) => log::warn!("scheduler gossip message rejected: {error}"),
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
            control_relay,
        })
    }

    /// Handle onto the converging member allowlist, so peer discovery can
    /// admit schedulers that were not reachable at startup.
    pub fn members(&self) -> MemberRegistry {
        self.members.clone()
    }

    /// Handle onto the peer control relay, so startup can install the
    /// forwarder it delivers through.
    pub fn control_relay(&self) -> AgentControlRelayHandler {
        self.control_relay.clone()
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
