use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use iroh::Endpoint;
use tokio::{sync::broadcast, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::{
    AttachmentManager, CapacityCriteria, GossipPublisher, QueryManager, SchedulerGossip,
    SchedulerIdentity, ValidatedMachineConfig,
};

const QUERY_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
const RELAY_GRANT_REFRESH_INTERVAL: Duration =
    Duration::from_secs(protocol::MAX_MACHINE_RELAY_GRANT_LIFETIME_SECS / 2);

#[derive(Clone)]
pub struct CapacityService {
    identity: Arc<SchedulerIdentity>,
    endpoint: Endpoint,
    queries: QueryManager,
    publisher: GossipPublisher,
    cancellation: CancellationToken,
}

impl CapacityService {
    pub async fn solicit(
        &self,
        criteria: CapacityCriteria,
    ) -> Result<Option<protocol::CapacityOffer>> {
        ensure!(
            !self.cancellation.is_cancelled(),
            "scheduler capacity service is shutting down"
        );
        let now = now_secs();
        let begun = self
            .queries
            .begin(criteria, &self.identity, &self.endpoint.addr(), now)
            .await?;
        if begun.newly_created
            && let Err(error) = self.publisher.publish(begun.query.clone()).await
        {
            self.queries.abort(&begun.query.query_id).await;
            return Err(error);
        }
        let wait = Duration::from_secs(begun.query.expires_at_secs.saturating_sub(now));
        tokio::select! {
            _ = self.cancellation.cancelled() => {
                anyhow::bail!("scheduler capacity request cancelled during shutdown")
            }
            _ = tokio::time::sleep(wait) => {}
        }
        Ok(self.queries.finish(&begun.query.query_id, now_secs()).await)
    }
}

pub struct CapacityCoordinator {
    cancellation: CancellationToken,
    task: JoinHandle<Result<()>>,
}

impl CapacityCoordinator {
    pub fn start(
        identity: SchedulerIdentity,
        endpoint: Endpoint,
        queries: QueryManager,
        attachments: AttachmentManager,
        gossip: &SchedulerGossip,
        machine_config: &ValidatedMachineConfig,
    ) -> (CapacityService, Self) {
        let mut query_events = gossip.subscribe_queries();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let cleanup_queries = queries.clone();
        let credential_identity = identity.clone();
        let credential_endpoint = endpoint.clone();
        let credential_config = machine_config.clone();
        let task = tokio::spawn(async move {
            let mut cleanup = tokio::time::interval(QUERY_CLEANUP_INTERVAL);
            cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut relay_refresh = tokio::time::interval(RELAY_GRANT_REFRESH_INTERVAL);
            relay_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            relay_refresh.tick().await;
            loop {
                tokio::select! {
                    _ = task_cancellation.cancelled() => return Ok(()),
                    _ = cleanup.tick() => cleanup_queries.cleanup(now_secs()).await,
                    _ = relay_refresh.tick() => {
                        // A relay that is briefly unreachable must not take the
                        // whole capacity plane down. The next tick retries, and
                        // existing grants stay valid until they expire.
                        if let Err(error) = credential_identity
                            .refresh_relay_credentials(
                                &credential_endpoint,
                                &credential_config,
                                now_secs(),
                            )
                            .await
                        {
                            log::warn!("refresh scheduler relay grants failed: {error:#}");
                        }
                    }
                    event = query_events.recv() => match event {
                        Ok(query) => {
                            // One malformed, expired, or undeliverable gossiped
                            // query must never end the coordinator. Dropping it
                            // only loses that one placement round.
                            if let Err(error) = attachments.fanout(&query).await {
                                log::warn!(
                                    "fan out capacity query {} failed: {error:#}",
                                    query.query_id
                                );
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(dropped)) => {
                            log::warn!("capacity query fanout lagged; dropped {dropped} queries");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Err(anyhow::anyhow!("capacity query event channel closed"));
                        }
                    }
                }
            }
        });
        let service = CapacityService {
            identity: Arc::new(identity),
            endpoint,
            queries,
            publisher: gossip.publisher(),
            cancellation: cancellation.clone(),
        };
        (service, Self { cancellation, task })
    }

    pub async fn join(&mut self) -> Result<Result<()>, tokio::task::JoinError> {
        (&mut self.task).await
    }

    pub async fn shutdown(self) -> Result<()> {
        self.cancellation.cancel();
        self.task
            .await
            .context("join capacity coordinator task")??;
        Ok(())
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
