mod envelope;
mod manifest;
mod messages;
mod sealed;
mod sidecar;
mod util;

pub use envelope::{
    Envelope, build_encrypted_envelope, build_encrypted_envelope_with_peer,
    build_envelope_canonical, build_envelope_canonical_with_peer, build_envelope_signed,
    build_envelope_signed_with_peer, envelope_extract_sig_pub,
    envelope_extract_sig_pub_legacy, root_as_envelope,
};

pub use sidecar::{
    SidecarManifestRequest, SidecarProviderRecordOwned, SidecarRouteKind, SidecarRouteSpec,
    build_sidecar_manifest_request, build_sidecar_provider_record, decode_sidecar_provider_record,
    root_as_sidecar_manifest_request, root_as_sidecar_provider_record,
};

pub use manifest::{
    AppliedManifest, KeyValue, OperationType, SignatureScheme, build_applied_manifest,
    root_as_applied_manifest,
};

pub use sealed::{
    SealedSpec, WorkloadSubmission, WorkloadDispatch,
    WorkloadSubmissionResponse, WorkloadAssignmentV2, CustodianInfo, CustodiansResponse,
    SubmittedShare,
    SEAL_VERSION_V1,
};

pub use messages::{
    ApplyRequest, ApplyResponse, CandidateNode, CandidatesResponse,
    CapabilityQuery, CapabilityReply, ResourceQuery, ResourceReply,
    CustodianAnnounce, CustodianWithdraw, HeartbeatPing,
    DeleteRequest, DeleteResponse, Handshake, Health, NodesResponse, TaskCreateResponse,
    TaskStatusResponse,
    custodian_topic_for_owner,
    build_manifest_target, compute_manifest_id, compute_manifest_id_from_content,
    extract_manifest_name, parse_peer_with_pubkey,
};

// ── Compatibility shims for old-style free functions ──────────────────────────

pub fn build_apply_request(
    replicas: u32,
    op_id: impl Into<String>,
    manifest_json: impl Into<String>,
    origin_peer: impl Into<String>,
    manifest_id: impl Into<String>,
) -> Vec<u8> {
    ApplyRequest {
        replicas,
        operation_id: op_id.into(),
        manifest_json: manifest_json.into(),
        origin_peer: origin_peer.into(),
        manifest_id: manifest_id.into(),
    }
    .to_bytes()
}

pub fn root_as_apply_request(bytes: &[u8]) -> Result<ApplyRequest, postcard::Error> {
    ApplyRequest::from_bytes(bytes)
}

pub fn build_apply_response(
    ok: bool,
    op_id: impl Into<String>,
    msg: impl Into<String>,
) -> Vec<u8> {
    ApplyResponse { ok, operation_id: op_id.into(), message: msg.into() }.to_bytes()
}

pub fn root_as_apply_response(bytes: &[u8]) -> Result<ApplyResponse, postcard::Error> {
    ApplyResponse::from_bytes(bytes)
}

pub fn build_delete_request(
    manifest_id: impl Into<String>,
    op_id: impl Into<String>,
    origin_peer: impl Into<String>,
    force: bool,
) -> Vec<u8> {
    DeleteRequest {
        manifest_id: manifest_id.into(),
        operation_id: op_id.into(),
        origin_peer: origin_peer.into(),
        force,
    }
    .to_bytes()
}

pub fn root_as_delete_request(bytes: &[u8]) -> Result<DeleteRequest, postcard::Error> {
    DeleteRequest::from_bytes(bytes)
}

pub fn build_delete_response(
    ok: bool,
    op_id: impl Into<String>,
    msg: impl Into<String>,
    manifest_id: impl Into<String>,
    removed: &[String],
) -> Vec<u8> {
    DeleteResponse {
        ok,
        operation_id: op_id.into(),
        message: msg.into(),
        manifest_id: manifest_id.into(),
        removed_workloads: removed.to_vec(),
    }
    .to_bytes()
}

pub fn root_as_delete_response(bytes: &[u8]) -> Result<DeleteResponse, postcard::Error> {
    DeleteResponse::from_bytes(bytes)
}

pub fn build_health(ok: bool, status: impl Into<String>) -> Vec<u8> {
    Health::new(ok, status).to_bytes()
}

pub fn root_as_health(bytes: &[u8]) -> Result<Health, postcard::Error> {
    Health::from_bytes(bytes)
}

pub fn build_nodes_response(peers: &[String]) -> Vec<u8> {
    NodesResponse::new(peers).to_bytes()
}

pub fn root_as_nodes_response(bytes: &[u8]) -> Result<NodesResponse, postcard::Error> {
    NodesResponse::from_bytes(bytes)
}

pub fn build_candidates_response_with_keys(ok: bool, candidates: &[(String, String)]) -> Vec<u8> {
    CandidatesResponse::with_keys(ok, candidates).to_bytes()
}

pub fn build_candidates_response(ok: bool, peer_ids: &[String]) -> Vec<u8> {
    CandidatesResponse::from_peer_ids(ok, peer_ids).to_bytes()
}

pub fn root_as_candidates_response(bytes: &[u8]) -> Result<CandidatesResponse, postcard::Error> {
    CandidatesResponse::from_bytes(bytes)
}

pub fn build_task_create_response(
    ok: bool,
    task_id: impl Into<String>,
    manifest_id: impl Into<String>,
    window_ms: u64,
) -> Vec<u8> {
    TaskCreateResponse::new(ok, task_id, manifest_id, window_ms).to_bytes()
}

pub fn root_as_task_create_response(bytes: &[u8]) -> Result<TaskCreateResponse, postcard::Error> {
    TaskCreateResponse::from_bytes(bytes)
}

pub fn build_task_status_response(
    task_id: impl Into<String>,
    state: impl Into<String>,
    peers: &[String],
    cid: Option<&str>,
) -> Vec<u8> {
    TaskStatusResponse::new(task_id, state, peers, cid).to_bytes()
}

pub fn root_as_task_status_response(bytes: &[u8]) -> Result<TaskStatusResponse, postcard::Error> {
    TaskStatusResponse::from_bytes(bytes)
}

pub fn build_capability_query(
    query_id: impl Into<String>,
    nonce: impl Into<String>,
    caps: &[&str],
    role: impl Into<String>,
    pubkey: impl Into<String>,
) -> Vec<u8> {
    CapabilityQuery::new(query_id, nonce, caps, role, pubkey).to_bytes()
}

pub fn root_as_capability_query(bytes: &[u8]) -> Result<CapabilityQuery, postcard::Error> {
    CapabilityQuery::from_bytes(bytes)
}

pub fn build_capability_reply(
    query_id: impl Into<String>,
    node_id: impl Into<String>,
    kem_pub: impl Into<String>,
    cert_bytes: Vec<u8>,
    caps: &[&str],
    role: impl Into<String>,
) -> Vec<u8> {
    CapabilityReply::new(query_id, node_id, kem_pub, cert_bytes, caps, role).to_bytes()
}

pub fn root_as_capability_reply(bytes: &[u8]) -> Result<CapabilityReply, postcard::Error> {
    CapabilityReply::from_bytes(bytes)
}

pub fn build_resource_query(
    query_id: impl Into<String>,
    cpu: u32,
    mem: u64,
    storage: u64,
    replicas: u32,
    role: impl Into<String>,
    pubkey: impl Into<String>,
) -> Vec<u8> {
    ResourceQuery::new(query_id, cpu, mem, storage, replicas, role, pubkey).to_bytes()
}

pub fn root_as_resource_query(bytes: &[u8]) -> Result<ResourceQuery, postcard::Error> {
    ResourceQuery::from_bytes(bytes)
}

#[allow(clippy::too_many_arguments)]
pub fn build_resource_reply(
    query_id: impl Into<String>,
    ok: bool,
    node_id: impl Into<String>,
    kem_pub: impl Into<String>,
    cpu: u32,
    mem: u64,
    storage: u64,
    reason: impl Into<String>,
    cert_bytes: Vec<u8>,
) -> Vec<u8> {
    ResourceReply::new(query_id, ok, node_id, kem_pub, cpu, mem, storage, reason, cert_bytes)
        .to_bytes()
}

pub fn root_as_resource_reply(bytes: &[u8]) -> Result<ResourceReply, postcard::Error> {
    ResourceReply::from_bytes(bytes)
}

pub fn build_handshake(
    nonce: u32,
    ts: u64,
    version: impl Into<String>,
    sig: impl Into<String>,
) -> Vec<u8> {
    Handshake {
        nonce,
        timestamp: ts,
        protocol_version: version.into(),
        signature: sig.into(),
        proxy_cert_b64: String::new(),
    }
    .to_bytes()
}

/// Build a handshake payload that carries an optional `proxy_cert_b64`.
/// When `proxy_cert_b64` is `None` the result is identical to [`build_handshake`].
pub fn build_handshake_with_cert(
    nonce: u32,
    ts: u64,
    version: impl Into<String>,
    sig: impl Into<String>,
    proxy_cert_b64: Option<&str>,
) -> Vec<u8> {
    Handshake {
        nonce,
        timestamp: ts,
        protocol_version: version.into(),
        signature: sig.into(),
        proxy_cert_b64: proxy_cert_b64.unwrap_or("").to_string(),
    }
    .to_bytes()
}

pub fn root_as_handshake(bytes: &[u8]) -> Result<Handshake, postcard::Error> {
    Handshake::from_bytes(bytes)
}

pub fn serialize_capability_query(q: &CapabilityQuery) -> Vec<u8> {
    q.to_bytes()
}
