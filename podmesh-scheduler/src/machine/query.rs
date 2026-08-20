use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use iroh::{EndpointAddr, EndpointId};
use protocol::{CapacityOffer, CapacityQuery};
use tokio::sync::{Mutex, watch};
use uuid::Uuid;

use super::{CapacityOfferHandler, SchedulerIdentity};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct CapacityCriteria {
    pub cpu_milli: u32,
    pub memory_bytes: u64,
    pub storage_bytes: u64,
    pub required_capabilities: Vec<String>,
    pub excluded_endpoint_ids: Vec<Vec<u8>>,
}

impl CapacityCriteria {
    pub fn normalized(mut self) -> Self {
        self.required_capabilities.sort_unstable();
        self.excluded_endpoint_ids.sort_unstable();
        self
    }
}

#[derive(Clone, Debug)]
pub struct BegunQuery {
    pub query: CapacityQuery,
    pub newly_created: bool,
    /// How many offers make this query answerable without waiting further.
    pub target_offers: usize,
    /// Live count of offers collected for this query.
    pub offers: watch::Receiver<usize>,
}

#[derive(Clone)]
pub struct QueryManager {
    inner: Arc<Mutex<QueryState>>,
    max_pending_queries: usize,
    max_offers_per_query: usize,
    query_timeout: Duration,
}

struct QueryState {
    pending: HashMap<String, PendingQuery>,
    equivalent: HashMap<CapacityCriteria, String>,
}

struct PendingQuery {
    criteria: CapacityCriteria,
    query: CapacityQuery,
    offers: HashMap<EndpointId, CapacityOffer>,
    selected: HashSet<EndpointId>,
    /// Offers that make this query answerable for all current waiters.
    target_offers: usize,
    /// Published on every accepted offer so waiters can stop early.
    offer_count: watch::Sender<usize>,
}

impl QueryManager {
    pub fn new(
        max_pending_queries: usize,
        max_offers_per_query: usize,
        query_timeout: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(QueryState {
                pending: HashMap::with_capacity(max_pending_queries),
                equivalent: HashMap::with_capacity(max_pending_queries),
            })),
            max_pending_queries,
            max_offers_per_query,
            query_timeout,
        }
    }

    /// Open a placement query, or join one already in flight for the same
    /// criteria.
    ///
    /// `target_offers` is how many offers the caller considers enough to choose
    /// from. Collecting every offer in a large mesh would mean waiting the full
    /// query lifetime on every placement, so the caller states what "enough"
    /// means for it and stops there.
    pub async fn begin(
        &self,
        criteria: CapacityCriteria,
        identity: &SchedulerIdentity,
        reply_address: &EndpointAddr,
        target_offers: usize,
        now_secs: u64,
    ) -> Result<BegunQuery> {
        let criteria = criteria.normalized();
        let target_offers = target_offers.clamp(1, self.max_offers_per_query);
        let mut state = self.inner.lock().await;
        cleanup_locked(&mut state, now_secs);
        if let Some(query_id) = state.equivalent.get(&criteria).cloned()
            && let Some(pending) = state.pending.get_mut(&query_id)
        {
            // Each joined caller needs its own selectable offer. Aggregating
            // demand keeps equivalent requests coalesced without forcing them
            // all onto one cached winner.
            pending.target_offers = pending
                .target_offers
                .saturating_add(target_offers)
                .min(self.max_offers_per_query);
            return Ok(BegunQuery {
                query: pending.query.clone(),
                newly_created: false,
                target_offers: pending.target_offers,
                offers: pending.offer_count.subscribe(),
            });
        }
        ensure!(
            state.pending.len() < self.max_pending_queries,
            "scheduler pending-query limit reached"
        );
        let lifetime_secs = self.query_timeout.as_secs();
        let expires_at_secs = now_secs.saturating_add(lifetime_secs);
        let reply_endpoint = identity.endpoint_record(reply_address, now_secs, expires_at_secs)?;
        let query = CapacityQuery {
            version: protocol::CAPACITY_PROTOCOL_VERSION,
            query_id: Uuid::new_v4().to_string(),
            nonce: Uuid::new_v4().to_string(),
            cpu_milli: criteria.cpu_milli,
            memory_bytes: criteria.memory_bytes,
            storage_bytes: criteria.storage_bytes,
            required_capabilities: criteria.required_capabilities.clone(),
            excluded_endpoint_ids: criteria.excluded_endpoint_ids.clone(),
            reply_endpoint,
            issued_at_secs: now_secs,
            expires_at_secs,
            signing_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(
            identity.signing_public(),
            identity.signing_private(),
            now_secs,
        )?;
        let (offer_count, offers) = watch::channel(0);
        state
            .equivalent
            .insert(criteria.clone(), query.query_id.clone());
        state.pending.insert(
            query.query_id.clone(),
            PendingQuery {
                criteria,
                query: query.clone(),
                offers: HashMap::with_capacity(target_offers),
                selected: HashSet::with_capacity(target_offers),
                target_offers,
                offer_count,
            },
        );
        Ok(BegunQuery {
            query,
            newly_created: true,
            target_offers,
            offers,
        })
    }

    pub async fn submit_offer(
        &self,
        offer: CapacityOffer,
        authenticated_endpoint: EndpointId,
        now_secs: u64,
    ) -> Result<bool> {
        offer.verify(now_secs)?;
        ensure!(
            offer.agent_endpoint.endpoint_id == authenticated_endpoint.as_bytes(),
            "capacity offer EndpointId does not match authenticated sender"
        );
        let mut state = self.inner.lock().await;
        cleanup_locked(&mut state, now_secs);
        let pending = state
            .pending
            .get_mut(&offer.query_id)
            .context("capacity offer names no active query")?;
        ensure!(
            pending.query.expires_at_secs >= now_secs,
            "capacity offer arrived after query deadline"
        );
        ensure!(
            !pending
                .criteria
                .excluded_endpoint_ids
                .iter()
                .any(|endpoint| endpoint == authenticated_endpoint.as_bytes()),
            "capacity offer came from an excluded agent"
        );
        ensure!(
            offer.available_cpu_milli >= pending.criteria.cpu_milli
                && offer.available_memory_bytes >= pending.criteria.memory_bytes
                && offer.available_storage_bytes >= pending.criteria.storage_bytes,
            "capacity offer does not satisfy requested resources"
        );
        let offered_capabilities: HashSet<_> =
            offer.capabilities.iter().map(String::as_str).collect();
        ensure!(
            pending
                .criteria
                .required_capabilities
                .iter()
                .all(|required| offered_capabilities.contains(required.as_str())),
            "capacity offer does not satisfy required capabilities"
        );
        if pending.offers.contains_key(&authenticated_endpoint) {
            return Ok(false);
        }
        ensure!(
            pending.offers.len() < self.max_offers_per_query,
            "capacity offer limit reached for query"
        );
        pending.offers.insert(authenticated_endpoint, offer);
        // Publishing the count is what lets a waiter return as soon as it has
        // enough to choose from, instead of sleeping out the query lifetime.
        let _ = pending.offer_count.send(pending.offers.len());
        Ok(true)
    }

    pub async fn finish(&self, query_id: &str, now_secs: u64) -> Option<CapacityOffer> {
        let mut state = self.inner.lock().await;
        let pending = state.pending.get_mut(query_id)?;
        let selected_endpoint = pending
            .offers
            .iter()
            .filter(|(endpoint, offer)| {
                offer.expires_at_secs >= now_secs && !pending.selected.contains(*endpoint)
            })
            .min_by(|(_, left), (_, right)| compare_offers(left, right, &pending.criteria))
            .map(|(endpoint, _)| *endpoint)?;
        pending.selected.insert(selected_endpoint);
        pending.offers.get(&selected_endpoint).cloned()
    }

    pub async fn abort(&self, query_id: &str) {
        let mut state = self.inner.lock().await;
        if let Some(pending) = state.pending.remove(query_id) {
            state.equivalent.remove(&pending.criteria);
        }
    }

    pub async fn cleanup(&self, now_secs: u64) {
        let mut state = self.inner.lock().await;
        cleanup_locked(&mut state, now_secs);
    }

    pub async fn pending_len(&self) -> usize {
        self.inner.lock().await.pending.len()
    }

    pub fn offer_handler(&self) -> CapacityOfferHandler {
        CapacityOfferHandler::new(self.clone(), self.query_timeout)
    }
}

fn cleanup_locked(state: &mut QueryState, now_secs: u64) {
    let expired: Vec<_> = state
        .pending
        .iter()
        .filter(|(_, pending)| pending.query.expires_at_secs.saturating_add(1) < now_secs)
        .map(|(query_id, pending)| (query_id.clone(), pending.criteria.clone()))
        .collect();
    for (query_id, criteria) in expired {
        state.pending.remove(&query_id);
        state.equivalent.remove(&criteria);
    }
}

fn compare_offers(
    left: &CapacityOffer,
    right: &CapacityOffer,
    criteria: &CapacityCriteria,
) -> std::cmp::Ordering {
    left.available_cpu_milli
        .saturating_sub(criteria.cpu_milli)
        .cmp(&right.available_cpu_milli.saturating_sub(criteria.cpu_milli))
        .then_with(|| {
            left.available_memory_bytes
                .saturating_sub(criteria.memory_bytes)
                .cmp(
                    &right
                        .available_memory_bytes
                        .saturating_sub(criteria.memory_bytes),
                )
        })
        .then_with(|| {
            left.available_storage_bytes
                .saturating_sub(criteria.storage_bytes)
                .cmp(
                    &right
                        .available_storage_bytes
                        .saturating_sub(criteria.storage_bytes),
                )
        })
        .then_with(|| {
            left.agent_endpoint
                .endpoint_id
                .cmp(&right.agent_endpoint.endpoint_id)
        })
}

#[cfg(test)]
mod query_tests;
