//! Finding which scheduler holds an agent's attachment.
//!
//! A client may address any scheduler, but an agent is attached to only a few.
//! Resolving that by probing every peer costs one QUIC connection per peer, so a
//! mesh of a few thousand schedulers would spend thousands of connections
//! answering a single client request — and every request would pay it again.
//!
//! Instead the asking scheduler broadcasts one gossiped query and the scheduler
//! that actually holds the attachment answers directly. The answer is then
//! cached, so the common case — a client running several operations against the
//! same agent — costs nothing after the first.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use iroh::EndpointId;
use tokio::sync::{Mutex, watch};

/// How long a learned location stays usable before it is confirmed again.
///
/// An agent moves only when it re-attaches, which is rare, but a stale entry
/// costs a failed forward — so the entry is short-lived enough that a move is
/// noticed quickly and long enough that a burst of operations pays once.
pub const LOCATION_CACHE_TTL: Duration = Duration::from_secs(60);

/// Locations remembered at once. Bounds memory on a scheduler that has relayed
/// for many agents.
pub const MAX_CACHED_LOCATIONS: usize = 16_384;

/// Location queries awaiting an answer at once.
pub const MAX_PENDING_LOCATIONS: usize = 1_024;

#[derive(Clone, Copy)]
struct CachedLocation {
    holder: EndpointId,
    learned_at: Instant,
}

#[derive(Default)]
struct LocationState {
    /// Agent to the scheduler last known to hold it.
    cache: HashMap<EndpointId, CachedLocation>,
    /// Insertion order, so the cache can be trimmed without scanning.
    order: std::collections::VecDeque<EndpointId>,
    /// Location queries in flight, by query id.
    pending: HashMap<String, watch::Sender<Option<EndpointId>>>,
}

/// Tracks in-flight location queries and remembers the answers.
#[derive(Clone, Default)]
pub struct LocationRegistry {
    inner: Arc<Mutex<LocationState>>,
}

impl LocationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The scheduler last known to hold `agent`, if that is still fresh.
    pub async fn cached(&self, agent: EndpointId) -> Option<EndpointId> {
        let state = self.inner.lock().await;
        let entry = state.cache.get(&agent)?;
        (entry.learned_at.elapsed() < LOCATION_CACHE_TTL).then_some(entry.holder)
    }

    /// Remember that `holder` has `agent` attached.
    pub async fn remember(&self, agent: EndpointId, holder: EndpointId) {
        let mut state = self.inner.lock().await;
        if state
            .cache
            .insert(
                agent,
                CachedLocation {
                    holder,
                    learned_at: Instant::now(),
                },
            )
            .is_none()
        {
            state.order.push_back(agent);
        }
        while state.order.len() > MAX_CACHED_LOCATIONS {
            if let Some(evicted) = state.order.pop_front() {
                state.cache.remove(&evicted);
            }
        }
    }

    /// Forget a location that turned out to be wrong.
    ///
    /// Called when a forward to the cached holder fails, so one stale entry
    /// cannot keep failing every subsequent request for that agent.
    pub async fn forget(&self, agent: EndpointId) {
        self.inner.lock().await.cache.remove(&agent);
    }

    /// Register a query and return the channel its answer arrives on.
    pub async fn begin(&self, query_id: &str) -> Result<watch::Receiver<Option<EndpointId>>> {
        let mut state = self.inner.lock().await;
        anyhow::ensure!(
            state.pending.len() < MAX_PENDING_LOCATIONS,
            "scheduler pending-location limit reached"
        );
        let (sender, receiver) = watch::channel(None);
        state.pending.insert(query_id.to_string(), sender);
        Ok(receiver)
    }

    /// Record an answer, waking whoever asked.
    pub async fn resolve(&self, query_id: &str, agent: EndpointId, holder: EndpointId) {
        let sender = {
            let state = self.inner.lock().await;
            state.pending.get(query_id).cloned()
        };
        if let Some(sender) = sender {
            let _ = sender.send(Some(holder));
            self.remember(agent, holder).await;
        }
    }

    pub async fn finish(&self, query_id: &str) {
        self.inner.lock().await.pending.remove(query_id);
    }
}

/// Answers gossiped location queries for the agents this scheduler holds.
#[derive(Clone)]
pub struct LocationResponder {
    endpoint: iroh::Endpoint,
    operation_timeout: Duration,
}

impl LocationResponder {
    pub fn new(endpoint: iroh::Endpoint, operation_timeout: Duration) -> Self {
        Self {
            endpoint,
            operation_timeout,
        }
    }

    /// Tell the asking scheduler that this one holds the attachment.
    ///
    /// The answer travels over the existing control-relay protocol in the
    /// opposite direction, so the asker authenticates the answering peer the
    /// same way it authenticates any relay traffic.
    pub async fn answer(&self, query: &protocol::AgentLocationQuery) -> Result<()> {
        let address = iroh_support::endpoint_addr(&query.reply_endpoint, now_secs())
            .context("agent location reply endpoint is unusable")?;
        let connection = tokio::time::timeout(
            self.operation_timeout,
            self.endpoint
                .connect(address, protocol::AGENT_CONTROL_RELAY_ALPN),
        )
        .await
        .context("agent location answer timed out")?
        .context("connect agent location asker")?;
        let request = protocol::AgentControlRelayRequest::location_answer(
            query.agent_endpoint_id.clone(),
            query.query_id.clone(),
        );
        let (mut send, _recv) = tokio::time::timeout(self.operation_timeout, connection.open_bi())
            .await
            .context("agent location answer stream timed out")?
            .context("open agent location answer stream")?;
        send.write_all(&request.to_bytes()?)
            .await
            .context("write agent location answer")?;
        send.finish().context("finish agent location answer")?;
        let _ = tokio::time::timeout(self.operation_timeout, connection.closed()).await;
        Ok(())
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint_id(seed: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    #[tokio::test]
    async fn a_learned_location_is_served_from_cache() {
        let registry = LocationRegistry::new();
        assert!(registry.cached(endpoint_id(1)).await.is_none());
        registry.remember(endpoint_id(1), endpoint_id(2)).await;
        assert_eq!(registry.cached(endpoint_id(1)).await, Some(endpoint_id(2)));
    }

    /// A stale entry must not keep failing every request for that agent.
    #[tokio::test]
    async fn a_wrong_location_can_be_forgotten() {
        let registry = LocationRegistry::new();
        registry.remember(endpoint_id(1), endpoint_id(2)).await;
        registry.forget(endpoint_id(1)).await;
        assert!(registry.cached(endpoint_id(1)).await.is_none());
    }

    #[tokio::test]
    async fn an_answer_wakes_the_asker_and_is_cached() {
        let registry = LocationRegistry::new();
        let mut answer = registry.begin("query-1").await.unwrap();
        registry
            .resolve("query-1", endpoint_id(1), endpoint_id(3))
            .await;
        assert_eq!(*answer.borrow_and_update(), Some(endpoint_id(3)));
        assert_eq!(registry.cached(endpoint_id(1)).await, Some(endpoint_id(3)));
        registry.finish("query-1").await;
    }

    /// An answer naming a query nobody asked for must be ignored, or any member
    /// could plant cache entries.
    #[tokio::test]
    async fn an_unsolicited_answer_is_ignored() {
        let registry = LocationRegistry::new();
        registry
            .resolve("never-asked", endpoint_id(1), endpoint_id(3))
            .await;
        assert!(registry.cached(endpoint_id(1)).await.is_none());
    }

    #[tokio::test]
    async fn the_cache_is_bounded() {
        let registry = LocationRegistry::new();
        for seed in 0..=u8::MAX {
            registry.remember(endpoint_id(seed), endpoint_id(1)).await;
        }
        assert!(registry.inner.lock().await.cache.len() <= MAX_CACHED_LOCATIONS);
    }
}
