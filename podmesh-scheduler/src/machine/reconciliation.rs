use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;
use tokio::sync::{Mutex, Notify};

pub const MAX_RECONCILIATION_AGENTS: usize = 2_048;
const MAX_PENDING_RECONCILIATIONS: usize = 32;
const MAX_RECONCILIATION_ANSWER_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOTAL_RECONCILIATION_ANSWER_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ReconciliationAnswer {
    pub agent_endpoint_id: EndpointId,
    pub sealed_response: Vec<u8>,
}

#[derive(Debug)]
pub struct ReconciliationOutcome {
    pub answers: Vec<ReconciliationAnswer>,
    pub unreachable_agents: Vec<EndpointId>,
    pub unreachable_schedulers: Vec<EndpointId>,
}

#[derive(Debug)]
struct Pending {
    expected: HashSet<EndpointId>,
    completed: HashSet<EndpointId>,
    answers: HashMap<EndpointId, Vec<u8>>,
    unreachable_agents: HashSet<EndpointId>,
    answer_bytes: usize,
    notify: Arc<Notify>,
}

#[derive(Debug)]
struct RegistryState {
    pending: HashMap<String, Pending>,
    answer_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct ReconciliationRegistry {
    state: Arc<Mutex<RegistryState>>,
    max_pending: usize,
}

impl ReconciliationRegistry {
    pub fn new(max_pending: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState {
                pending: HashMap::with_capacity(max_pending.min(MAX_PENDING_RECONCILIATIONS)),
                answer_bytes: 0,
            })),
            max_pending: max_pending.min(MAX_PENDING_RECONCILIATIONS),
        }
    }

    pub(super) async fn begin(
        &self,
        query_id: String,
        expected: HashSet<EndpointId>,
    ) -> Result<Arc<Notify>> {
        let mut state = self.state.lock().await;
        ensure!(
            state.pending.len() < self.max_pending,
            "scheduler reconciliation limit reached"
        );
        ensure!(
            !state.pending.contains_key(&query_id),
            "duplicate scheduler reconciliation query id"
        );
        let notify = Arc::new(Notify::new());
        state.pending.insert(
            query_id,
            Pending {
                expected,
                completed: HashSet::new(),
                answers: HashMap::new(),
                unreachable_agents: HashSet::new(),
                answer_bytes: 0,
                notify: notify.clone(),
            },
        );
        Ok(notify)
    }

    pub async fn record(
        &self,
        authenticated_scheduler: EndpointId,
        response: protocol::SchedulerReconciliationResponse,
    ) -> Result<()> {
        let responder = decode_endpoint(&response.responder_endpoint.endpoint_id)?;
        ensure!(
            responder == authenticated_scheduler,
            "reconciliation response endpoint does not match authenticated scheduler"
        );
        let mut state = self.state.lock().await;
        let Some(request) = state.pending.get(&response.query_id) else {
            return Ok(());
        };
        ensure!(
            request.expected.contains(&authenticated_scheduler),
            "unexpected scheduler answered reconciliation query"
        );
        let event = response.event;
        if let protocol::ReconciliationEvent::AgentAnswer {
            agent_endpoint_id,
            sealed_response,
        } = &event
        {
            let agent = decode_endpoint(agent_endpoint_id)?;
            if !request.answers.contains_key(&agent) {
                ensure!(
                    request.answers.len() + request.unreachable_agents.len()
                        < MAX_RECONCILIATION_AGENTS,
                    "reconciliation agent answer limit reached"
                );
                ensure!(
                    request.answer_bytes.saturating_add(sealed_response.len())
                        <= MAX_RECONCILIATION_ANSWER_BYTES,
                    "reconciliation query answer byte limit reached"
                );
                ensure!(
                    state.answer_bytes.saturating_add(sealed_response.len())
                        <= MAX_TOTAL_RECONCILIATION_ANSWER_BYTES,
                    "scheduler reconciliation answer memory limit reached"
                );
            }
        }
        let mut added_bytes = 0;
        let notify = {
            let request = state
                .pending
                .get_mut(&response.query_id)
                .context("scheduler reconciliation state disappeared")?;
            match event {
                protocol::ReconciliationEvent::AgentAnswer {
                    agent_endpoint_id,
                    sealed_response,
                } => {
                    let agent = decode_endpoint(&agent_endpoint_id)?;
                    request.unreachable_agents.remove(&agent);
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        request.answers.entry(agent)
                    {
                        added_bytes = sealed_response.len();
                        request.answer_bytes += added_bytes;
                        entry.insert(sealed_response);
                    }
                }
                protocol::ReconciliationEvent::AgentUnreachable { agent_endpoint_id } => {
                    let agent = decode_endpoint(&agent_endpoint_id)?;
                    if !request.answers.contains_key(&agent) {
                        ensure!(
                            request.answers.len() + request.unreachable_agents.len()
                                < MAX_RECONCILIATION_AGENTS
                                || request.unreachable_agents.contains(&agent),
                            "reconciliation agent answer limit reached"
                        );
                        request.unreachable_agents.insert(agent);
                    }
                }
                protocol::ReconciliationEvent::Complete => {
                    request.completed.insert(authenticated_scheduler);
                }
            }
            request.notify.clone()
        };
        state.answer_bytes += added_bytes;
        drop(state);
        notify.notify_waiters();
        Ok(())
    }

    pub(super) async fn finish(
        &self,
        query_id: &str,
        notify: Arc<Notify>,
        timeout: Duration,
    ) -> Result<ReconciliationOutcome> {
        let wait = async {
            loop {
                let notified = notify.notified();
                let complete = {
                    let state = self.state.lock().await;
                    state
                        .pending
                        .get(query_id)
                        .is_none_or(|request| request.completed == request.expected)
                };
                if complete {
                    return;
                }
                notified.await;
            }
        };
        let _ = tokio::time::timeout(timeout, wait).await;
        let mut state = self.state.lock().await;
        let request = state
            .pending
            .remove(query_id)
            .context("scheduler reconciliation state disappeared")?;
        state.answer_bytes = state.answer_bytes.saturating_sub(request.answer_bytes);
        drop(state);
        let mut unreachable_schedulers = request
            .expected
            .difference(&request.completed)
            .copied()
            .collect::<Vec<_>>();
        unreachable_schedulers.sort_unstable_by_key(|endpoint| endpoint.as_bytes().to_vec());
        let mut unreachable_agents = request.unreachable_agents.into_iter().collect::<Vec<_>>();
        unreachable_agents.sort_unstable_by_key(|endpoint| endpoint.as_bytes().to_vec());
        let mut answers = request
            .answers
            .into_iter()
            .map(
                |(agent_endpoint_id, sealed_response)| ReconciliationAnswer {
                    agent_endpoint_id,
                    sealed_response,
                },
            )
            .collect::<Vec<_>>();
        answers.sort_unstable_by_key(|answer| answer.agent_endpoint_id.as_bytes().to_vec());
        Ok(ReconciliationOutcome {
            answers,
            unreachable_agents,
            unreachable_schedulers,
        })
    }
}

impl ReconciliationRegistry {
    pub(super) async fn cancel(&self, query_id: &str) {
        let mut state = self.state.lock().await;
        if let Some(request) = state.pending.remove(query_id) {
            state.answer_bytes = state.answer_bytes.saturating_sub(request.answer_bytes);
        }
    }

    #[cfg(test)]
    pub(super) async fn is_empty(&self) -> bool {
        self.state.lock().await.pending.is_empty()
    }
}

fn decode_endpoint(bytes: &[u8]) -> Result<EndpointId> {
    let fixed: [u8; protocol::IROH_ENDPOINT_ID_BYTES] = bytes
        .try_into()
        .context("reconciliation EndpointId length is invalid")?;
    EndpointId::from_bytes(&fixed).context("reconciliation EndpointId is invalid")
}
