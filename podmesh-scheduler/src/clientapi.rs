//! HTTP API consumed by `podctl`.
//!
//! `podctl` is a short-lived CLI: it has no persistent Iroh endpoint and cannot
//! be dialed, so it speaks plain HTTP to whichever scheduler it can reach. The
//! scheduler answers placement questions from the mesh and relays the owner's
//! already-encrypted control payloads to the selected agent over Iroh.
//!
//! The scheduler stays stateless and blind: it never holds workload ciphertext,
//! DEKs, lifecycle state, or receipts. It only moves opaque owner-signed bytes.
//!
//! The API is deliberately unauthenticated — the mesh is open, and anyone with a
//! keypair may place a workload. It is not, however, unbounded: a selection
//! request is one cheap HTTP call that fans a signed query out to every agent in
//! the mesh, and a relay call opens a QUIC connection, so both are throttled per
//! peer address. That is a availability bound, not access control.

use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use protocol::{
    AgentControlOperation, MAX_AGENT_CONTROL_PAYLOAD_BYTES,
    capacity::{
        MAX_CAPACITY_CAPABILITIES, MAX_CAPACITY_CAPABILITY_LEN, MAX_CAPACITY_EXCLUDED_ENDPOINTS,
    },
};

use crate::machine::{
    AgentControlForwarder, CapacityCriteria, CapacityService, ForwardError, ReconciliationService,
    SchedulerIdentity,
};

const MAX_PLACEMENT_CPU_MILLI: u32 = 1_000_000;
const MAX_PLACEMENT_MEMORY_BYTES: u64 = 1024 * 1024 * 1024 * 1024 * 1024;
const MAX_PLACEMENT_STORAGE_BYTES: u64 = 1024 * 1024 * 1024 * 1024 * 1024;

/// Upper bound on a relayed owner payload, matched to the agent control frame.
const MAX_CLIENT_BODY_BYTES: usize = MAX_AGENT_CONTROL_PAYLOAD_BYTES;

/// Requests per minute a single peer address may make.
///
/// Sized against the busiest legitimate client: one `podctl apply` spends three
/// calls per replica and is capped at `MAX_WORKLOAD_REPLICAS` replicas, so a
/// maximal deployment costs a few hundred requests. The default leaves room for
/// several of those a minute while still bounding a caller that only wants to
/// enumerate agents or burn placement capacity.
pub const DEFAULT_CLIENT_RATE_LIMIT_PER_MINUTE: u32 = 1_200;

/// Offers a selection waits for before answering, when the client says nothing.
///
/// More offers means a better-informed choice; waiting for all of them means
/// every placement costs the full query lifetime. A handful is enough to avoid
/// packing every deployment onto whichever agent answers first.
pub const DEFAULT_TARGET_OFFERS: usize = 4;

/// Ceiling on the offers a client may ask a selection to wait for, so one
/// caller cannot make a query hold open for the full lifetime on purpose.
pub const MAX_TARGET_OFFERS: usize = 64;

/// Agents a single list broadcast will contact.
///
/// Listing exists to find workloads an owner lost track of, so it has to reach
/// every agent rather than a sample. The bound keeps one HTTP call from turning
/// into unbounded fan-out on a very large scheduler.
pub const MAX_LIST_BROADCAST_AGENTS: usize = 2_048;

/// Lifetime stamped on the `EndpointRecord` served over HTTP. Bootstrapping
/// peers must re-fetch after this expires, which keeps a stale address from
/// being pinned forever.
const PUBLISHED_ENDPOINT_RECORD_LIFETIME_SECS: u64 =
    protocol::endpoint_record::MAX_ENDPOINT_RECORD_LIFETIME_SECS;

#[derive(Clone)]
pub struct ClientApi {
    capacity: CapacityService,
    forwarder: AgentControlForwarder,
    identity: SchedulerIdentity,
    endpoint: iroh::Endpoint,
    reconciliation: Option<ReconciliationService>,
    rate_limit_per_minute: u32,
}

impl ClientApi {
    pub fn new(
        capacity: CapacityService,
        forwarder: AgentControlForwarder,
        identity: SchedulerIdentity,
        endpoint: iroh::Endpoint,
    ) -> Self {
        Self {
            capacity,
            forwarder,
            identity,
            endpoint,
            reconciliation: None,
            rate_limit_per_minute: DEFAULT_CLIENT_RATE_LIMIT_PER_MINUTE,
        }
    }

    pub fn with_reconciliation(mut self, reconciliation: ReconciliationService) -> Self {
        self.reconciliation = Some(reconciliation);
        self
    }

    /// Override the per-peer request budget. Zero disables throttling, which is
    /// appropriate only where every caller shares one source address.
    pub fn with_rate_limit(mut self, requests_per_minute: u32) -> Self {
        self.rate_limit_per_minute = requests_per_minute;
        self
    }

    /// Build the HTTP router.
    ///
    /// The result carries per-peer middleware, so it must be served through
    /// [`axum_support::with_connect_info`]. Handing the bare router to
    /// `axum::serve` leaves the peer address unavailable and every throttled
    /// route answers 500.
    pub fn router(self) -> Router {
        let rate_limit_per_minute = self.rate_limit_per_minute;
        let router = Router::new()
            .route("/api/v1/endpoint_record", get(get_endpoint_record))
            .route("/api/v1/agents/select", get(select_agent))
            .route("/api/v1/agents/{agent}/admission", post(post_admission))
            .route("/api/v1/agents/{agent}/deploy", post(post_deploy))
            .route("/api/v1/agents/{agent}/update", post(post_update))
            .route("/api/v1/agents/{agent}/command", post(post_command))
            .route("/api/v1/workloads/list", post(post_workload_list))
            .layer(DefaultBodyLimit::max(MAX_CLIENT_BODY_BYTES))
            .with_state(self);

        // Liveness probes are deliberately outside the limiter: an orchestrator
        // polling health must never be throttled by unrelated client traffic,
        // and the answer is a constant.
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/ready", get(|| async { "ready" }))
            .merge(axum_support::with_rate_limit(router, rate_limit_per_minute))
    }
}

/// Signed reachability record for this scheduler's Iroh endpoint.
#[derive(serde::Serialize)]
struct EndpointRecordResponse {
    endpoint_record_b64: String,
    endpoint_id: String,
    signing_pubkey_b64: String,
}

/// Publishes this scheduler's signed `EndpointRecord`.
///
/// Bootstrapping over Iroh is a chicken-and-egg problem: an agent cannot dial a
/// scheduler whose address it does not know. This endpoint breaks the cycle
/// without weakening the trust model, because the record is signed by the
/// scheduler's own key and self-expiring. A hostile HTTP intermediary can
/// withhold the record but cannot forge a usable one.
async fn get_endpoint_record(
    State(api): State<ClientApi>,
) -> ApiResult<Json<EndpointRecordResponse>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "scheduler clock is before the unix epoch".to_string(),
            )
        })?
        .as_secs();
    let expires = now.saturating_add(PUBLISHED_ENDPOINT_RECORD_LIFETIME_SECS);
    let record = api
        .identity
        .endpoint_record(&api.endpoint.addr(), now, expires)
        .and_then(|record| record.to_bytes(now))
        .map_err(|error| {
            log::warn!("publish scheduler endpoint record failed: {error:#}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "scheduler endpoint record is not available yet".to_string(),
            )
        })?;
    Ok(Json(EndpointRecordResponse {
        endpoint_record_b64: crypto::b64_encode(&record),
        endpoint_id: hex::encode(api.endpoint.id().as_bytes()),
        signing_pubkey_b64: crypto::b64_encode(api.identity.signing_public()),
    }))
}

type ApiResult<T> = Result<T, (StatusCode, String)>;

/// Query parameters for `GET /api/v1/agents/select`.
#[derive(Debug, Default, serde::Deserialize)]
struct SelectQuery {
    /// Comma-separated lowercase hex EndpointIds that must not be offered.
    /// A client spreading replicas passes the agents it already occupies so the
    /// mesh answers with a different one.
    #[serde(default)]
    exclude: Option<String>,
    /// How many offers to collect before answering.
    ///
    /// A client placing several replicas wants to choose between at least that
    /// many agents; one placing a single workload does not need to wait for the
    /// whole mesh to answer.
    #[serde(default)]
    candidates: Option<usize>,
    cpu_milli: u32,
    memory_bytes: u64,
    storage_bytes: u64,
    /// Comma-separated capabilities every offered agent must advertise.
    #[serde(default)]
    capabilities: Option<String>,
}

async fn select_agent(
    State(api): State<ClientApi>,
    Query(query): Query<SelectQuery>,
) -> ApiResult<Json<protocol::CapacityOffer>> {
    validate_placement_resources(query.cpu_milli, query.memory_bytes, query.storage_bytes)?;
    let criteria = CapacityCriteria {
        cpu_milli: query.cpu_milli,
        memory_bytes: query.memory_bytes,
        storage_bytes: query.storage_bytes,
        required_capabilities: parse_capabilities(query.capabilities.as_deref())?,
        excluded_endpoint_ids: parse_exclusions(query.exclude.as_deref())?,
    };
    let target_offers = query
        .candidates
        .unwrap_or(DEFAULT_TARGET_OFFERS)
        .clamp(1, MAX_TARGET_OFFERS);
    match api.capacity.solicit(criteria, target_offers).await {
        Ok(Some(offer)) => Ok(Json(offer)),
        Ok(None) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no agent capacity available".into(),
        )),
        Err(error) => {
            log::error!("capacity solicitation failed: {error}");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "capacity selection failed".into(),
            ))
        }
    }
}

fn validate_placement_resources(
    cpu_milli: u32,
    memory_bytes: u64,
    storage_bytes: u64,
) -> ApiResult<()> {
    if cpu_milli == 0
        || cpu_milli > MAX_PLACEMENT_CPU_MILLI
        || memory_bytes == 0
        || memory_bytes > MAX_PLACEMENT_MEMORY_BYTES
        || storage_bytes == 0
        || storage_bytes > MAX_PLACEMENT_STORAGE_BYTES
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "placement resources are outside supported bounds".to_string(),
        ));
    }
    Ok(())
}

fn parse_capabilities(raw: Option<&str>) -> ApiResult<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut capabilities = Vec::new();
    for capability in raw.split(',').map(str::trim) {
        if capability.is_empty()
            || capability.len() > MAX_CAPACITY_CAPABILITY_LEN
            || !capability
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err((
                StatusCode::BAD_REQUEST,
                "placement capability is invalid".to_string(),
            ));
        }
        if capabilities.len() >= MAX_CAPACITY_CAPABILITIES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("at most {MAX_CAPACITY_CAPABILITIES} capabilities are accepted"),
            ));
        }
        if capabilities.iter().any(|existing| existing == capability) {
            return Err((
                StatusCode::BAD_REQUEST,
                "placement capabilities must be unique".to_string(),
            ));
        }
        capabilities.push(capability.to_string());
    }
    Ok(capabilities)
}

fn parse_exclusions(raw: Option<&str>) -> ApiResult<Vec<Vec<u8>>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut excluded = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        if excluded.len() >= MAX_CAPACITY_EXCLUDED_ENDPOINTS {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("at most {MAX_CAPACITY_EXCLUDED_ENDPOINTS} exclusions are accepted"),
            ));
        }
        excluded.push(parse_agent_id(entry)?.as_bytes().to_vec());
    }
    Ok(excluded)
}

async fn post_admission(
    State(api): State<ClientApi>,
    Path(agent): Path<String>,
    body: Bytes,
) -> ApiResult<Vec<u8>> {
    relay(api, agent, AgentControlOperation::Admission, body).await
}

async fn post_deploy(
    State(api): State<ClientApi>,
    Path(agent): Path<String>,
    body: Bytes,
) -> ApiResult<Vec<u8>> {
    relay(api, agent, AgentControlOperation::Deploy, body).await
}

async fn post_update(
    State(api): State<ClientApi>,
    Path(agent): Path<String>,
    body: Bytes,
) -> ApiResult<Vec<u8>> {
    relay(api, agent, AgentControlOperation::Update, body).await
}

async fn post_command(
    State(api): State<ClientApi>,
    Path(agent): Path<String>,
    body: Bytes,
) -> ApiResult<Vec<u8>> {
    relay(api, agent, AgentControlOperation::Command, body).await
}

/// One agent's answer to a broadcast list request.
#[derive(serde::Serialize)]
struct WorkloadListEntry {
    /// Lowercase hex EndpointId of the agent that answered.
    agent_endpoint_id: String,
    /// The agent's owner-sealed `WorkloadListResponse`, base64 encoded.
    ///
    /// The scheduler relays it unopened: it holds no tenant key and must not
    /// learn which workloads an owner is running.
    response_b64: String,
}

#[derive(serde::Serialize)]
struct WorkloadListReply {
    answered: Vec<WorkloadListEntry>,
    /// Agents that did not answer, so a caller knows its view is partial rather
    /// than concluding a workload is gone.
    unreachable_agents: Vec<String>,
    /// Schedulers in the admitted mesh that did not complete before the
    /// request deadline. Their agents, if any, are outside this view.
    unreachable_schedulers: Vec<String>,
}

/// Broadcast an owner-signed list request to every attached agent.
///
/// `podctl` keeps the only index of where it placed replicas, so this is how an
/// owner finds workloads after losing or outdating that index — which means it
/// cannot name the agents to ask, and the scheduler must reach all of them.
///
/// The request body is the signed request verbatim. It is not sealed, because
/// it carries nothing secret; each agent's answer is sealed to the owner, so the
/// scheduler learns nothing from relaying it.
async fn post_workload_list(
    State(api): State<ClientApi>,
    body: Bytes,
) -> ApiResult<Json<WorkloadListReply>> {
    if body.is_empty() || body.len() > protocol::MAX_WORKLOAD_LIST_REQUEST_BYTES {
        return Err((
            StatusCode::BAD_REQUEST,
            "list request size is invalid".to_string(),
        ));
    }
    // Verified here only to refuse obvious junk before fanning out; the agents
    // verify it again, and they are the ones that decide what to answer.
    protocol::WorkloadListRequest::from_bytes(&body, crate::now_secs()).map_err(|error| {
        log::debug!("refusing malformed workload list request: {error:#}");
        (
            StatusCode::BAD_REQUEST,
            "invalid workload list request".to_string(),
        )
    })?;

    let reconciliation = api.reconciliation.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "workload reconciliation is not initialized".to_string(),
        )
    })?;
    let outcome = reconciliation
        .reconcile(body.to_vec())
        .await
        .map_err(|error| {
            log::warn!("mesh-wide workload reconciliation failed: {error:#}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "workload reconciliation failed".to_string(),
            )
        })?;
    let answered = outcome
        .answers
        .into_iter()
        .map(|answer| WorkloadListEntry {
            agent_endpoint_id: hex::encode(answer.agent_endpoint_id.as_bytes()),
            response_b64: crypto::b64_encode(&answer.sealed_response),
        })
        .collect();
    Ok(Json(WorkloadListReply {
        answered,
        unreachable_agents: outcome
            .unreachable_agents
            .into_iter()
            .map(|endpoint| hex::encode(endpoint.as_bytes()))
            .collect(),
        unreachable_schedulers: outcome
            .unreachable_schedulers
            .into_iter()
            .map(|endpoint| hex::encode(endpoint.as_bytes()))
            .collect(),
    }))
}

async fn relay(
    api: ClientApi,
    agent: String,
    operation: AgentControlOperation,
    body: Bytes,
) -> ApiResult<Vec<u8>> {
    let agent = parse_agent_id(&agent)?;
    if body.is_empty() || body.len() > MAX_CLIENT_BODY_BYTES {
        return Err((
            StatusCode::BAD_REQUEST,
            "encrypted payload size is invalid".to_string(),
        ));
    }
    api.forwarder
        .forward(agent, operation, body.to_vec())
        .await
        .map_err(|error| {
            let status = match error {
                ForwardError::UnknownAgent => StatusCode::NOT_FOUND,
                ForwardError::Busy => StatusCode::SERVICE_UNAVAILABLE,
                ForwardError::Rejected => StatusCode::BAD_REQUEST,
                ForwardError::Unavailable => StatusCode::BAD_GATEWAY,
            };
            (status, error.to_string())
        })
}

/// Agent EndpointIds travel as lowercase hex so that `podctl` never needs to
/// link Iroh just to address an agent.
fn parse_agent_id(raw: &str) -> ApiResult<iroh::EndpointId> {
    let invalid = || {
        (
            StatusCode::BAD_REQUEST,
            "invalid agent EndpointId".to_string(),
        )
    };
    if raw.len() != protocol::IROH_ENDPOINT_ID_BYTES * 2 {
        return Err(invalid());
    }
    let mut bytes = [0u8; protocol::IROH_ENDPOINT_ID_BYTES];
    hex::decode_to_slice(raw, &mut bytes).map_err(|_| invalid())?;
    iroh::EndpointId::from_bytes(&bytes).map_err(|_| invalid())
}

/// Maximum time the scheduler will spend relaying one owner request.
pub const CLIENT_RELAY_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum number of owner requests relayed to agents at the same time.
pub const MAX_CONCURRENT_CLIENT_RELAYS: usize = 64;
