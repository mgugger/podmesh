//! Serving relay credentials over the REST API.
//!
//! Two audiences with different exposure share this file so the difference is
//! impossible to miss. Tenants receive only the token derived for the tenant
//! they name; peer proxies receive the mesh secret those tokens derive from,
//! which is a mesh-wide credential and is why publishing is opt-in.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Serialize;

use crate::restapi::{ApiError, RestState};

/// Response body for `GET /api/v1/workload_relay_bootstrap`.
#[derive(Serialize)]
struct WorkloadRelayBootstrapResponse {
    /// Base64-postcard `EndpointRecord` identifying this proxy.
    endpoint_record_b64: String,
    /// Relay token for the tenant named in the request.
    auth_token: String,
    /// Base64 DER of the relay's certificate, pinned by sidecars.
    ca_certificate_b64: String,
}

/// Response body for `GET /api/v1/workload_relay_mesh_secret`.
#[derive(Serialize)]
struct WorkloadRelayMeshSecretResponse {
    /// Secret from which every tenant's relay token is derived.
    mesh_secret: String,
    /// Base64 DER of the relay's certificate, pinned by sidecars.
    ca_certificate_b64: String,
}

/// Serves the mesh secret to peer proxies.
///
/// This is the proxy-to-proxy half of relay bootstrap and hands out a
/// mesh-wide credential, so it is behind the same opt-in as the tenant half and
/// belongs only on a trusted network. Tenants use
/// `GET /api/v1/workload_relay_bootstrap`, which never discloses this.
pub(crate) async fn get_workload_relay_mesh_secret(
    State(state): State<RestState>,
) -> impl IntoResponse {
    let Some(bootstrap) = state.relay_bootstrap.clone() else {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: "this proxy does not publish workload relay credentials".into(),
            }),
        )
            .into_response();
    };
    (
        StatusCode::OK,
        Json(WorkloadRelayMeshSecretResponse {
            mesh_secret: bootstrap.mesh_secret,
            ca_certificate_b64: crypto::b64_encode(&bootstrap.ca_certificate_der),
        }),
    )
        .into_response()
}

/// Query for `GET /api/v1/workload_relay_bootstrap`.
#[derive(serde::Deserialize)]
pub(crate) struct WorkloadRelayBootstrapQuery {
    /// Base64 Ed25519 public key of the namespace owner asking.
    owner: String,
}

pub(crate) async fn get_workload_relay_bootstrap(
    State(state): State<RestState>,
    Query(query): Query<WorkloadRelayBootstrapQuery>,
) -> impl IntoResponse {
    let Some(bootstrap) = state.relay_bootstrap.clone() else {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: "this proxy does not publish workload relay credentials".into(),
            }),
        )
            .into_response();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let record = match state.endpoint_record.read() {
        Ok(record) => record.clone(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: "proxy EndpointRecord lock poisoned".into(),
                }),
            )
                .into_response();
        }
    };
    let auth_token = match protocol::derive_tenant_relay_token(&bootstrap.mesh_secret, &query.owner)
    {
        Ok(token) => token,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!("cannot derive a relay token for that tenant: {error}"),
                }),
            )
                .into_response();
        }
    };
    match record.to_bytes(now) {
        Ok(bytes) => (
            StatusCode::OK,
            Json(WorkloadRelayBootstrapResponse {
                endpoint_record_b64: crypto::b64_encode(&bytes),
                auth_token,
                ca_certificate_b64: crypto::b64_encode(&bootstrap.ca_certificate_der),
            }),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: format!("failed to encode EndpointRecord: {error}"),
            }),
        )
            .into_response(),
    }
}
