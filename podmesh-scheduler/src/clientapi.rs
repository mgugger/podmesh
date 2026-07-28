//! HTTP API consumed by `podctl`.
//!
//! `podctl` is a short-lived CLI: it has no persistent Iroh endpoint and cannot
//! be dialed, so it speaks plain HTTP to whichever scheduler it can reach. The
//! scheduler answers placement questions from the mesh and relays the owner's
//! already-encrypted control payloads to the selected agent over Iroh.
//!
//! The scheduler stays stateless and blind: it never holds workload ciphertext,
//! DEKs, lifecycle state, or receipts. It only moves opaque owner-signed bytes.

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
    capacity::MAX_CAPACITY_EXCLUDED_ENDPOINTS,
};

use crate::machine::{
    AgentControlForwarder, CapacityCriteria, CapacityService, ForwardError, SchedulerIdentity,
};

/// Placement solicited by `podctl` currently ignores per-workload resource
/// requirements; the agent re-validates every reservation during admission.
const PLACEMENT_PROBE_CPU_MILLI: u32 = 1;
const PLACEMENT_PROBE_MEMORY_BYTES: u64 = 1;
const PLACEMENT_PROBE_STORAGE_BYTES: u64 = 1;

/// Upper bound on a relayed owner payload, matched to the agent control frame.
const MAX_CLIENT_BODY_BYTES: usize = MAX_AGENT_CONTROL_PAYLOAD_BYTES;

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
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/ready", get(|| async { "ready" }))
            .route("/api/v1/endpoint_record", get(get_endpoint_record))
            .route("/api/v1/agents/select", get(select_agent))
            .route("/api/v1/agents/{agent}/admission", post(post_admission))
            .route("/api/v1/agents/{agent}/deploy", post(post_deploy))
            .route("/api/v1/agents/{agent}/command", post(post_command))
            .layer(DefaultBodyLimit::max(MAX_CLIENT_BODY_BYTES))
            .with_state(self)
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
}

async fn select_agent(
    State(api): State<ClientApi>,
    Query(query): Query<SelectQuery>,
) -> ApiResult<Json<protocol::CapacityOffer>> {
    let criteria = CapacityCriteria {
        cpu_milli: PLACEMENT_PROBE_CPU_MILLI,
        memory_bytes: PLACEMENT_PROBE_MEMORY_BYTES,
        storage_bytes: PLACEMENT_PROBE_STORAGE_BYTES,
        required_capabilities: Vec::new(),
        excluded_endpoint_ids: parse_exclusions(query.exclude.as_deref())?,
    };
    match api.capacity.solicit(criteria).await {
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

async fn post_command(
    State(api): State<ClientApi>,
    Path(agent): Path<String>,
    body: Bytes,
) -> ApiResult<Vec<u8>> {
    relay(api, agent, AgentControlOperation::Command, body).await
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
