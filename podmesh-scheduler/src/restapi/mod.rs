use crate::runtime::{PortMapping, RuntimeEngine, RuntimeError, WorkloadInfo, WorkloadStatus};
use crate::scheduler::{
    NodeCandidate, NodeCapabilities, Scheduler, SchedulerConfig, SchedulingStrategy,
};
use axum::{
    Router,
    body::Bytes,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware,
    routing::{delete, get, post},
};
use base64::Engine;
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use protocol::libp2p_constants::{FREE_CAPACITY_PREFIX, FREE_CAPACITY_TIMEOUT_MS};
use protocol::machine::parse_peer_with_pubkey;

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::{sync::watch, time::Duration};

pub mod envelope_handler;
use envelope_handler::{
    EnvelopeHandler, create_encrypted_response_with_key, create_response_for_envelope_metadata,
    create_response_with_fallback,
};

async fn get_nodes(
    State(state): State<RestState>,
    _headers: HeaderMap,
) -> Result<axum::response::Response<axum::body::Body>, axum::http::StatusCode> {
    let peers = state.peer_rx.borrow().clone();
    let response_data = protocol::machine::build_nodes_response(&peers);

    // No envelope metadata available, return unencrypted response
    create_response_with_fallback(&response_data).await
}

async fn get_kem_public_key(State(_state): State<RestState>) -> String {
    // Get the machine's KEM public key for encryption
    match crypto::ensure_kem_keypair_on_disk() {
        Ok((kem_pub_bytes, _)) => base64::engine::general_purpose::STANDARD.encode(&kem_pub_bytes),
        Err(e) => format!("ERROR: Failed to get KEM public key: {}", e),
    }
}

async fn get_signing_public_key(State(_state): State<RestState>) -> String {
    // Get the machine's signing public key for signature verification
    match crypto::ensure_keypair_on_disk() {
        Ok((signing_pub_bytes, _)) => {
            base64::engine::general_purpose::STANDARD.encode(&signing_pub_bytes)
        }
        Err(e) => format!("ERROR: Failed to get signing public key: {}", e),
    }
}

#[derive(Clone)]
pub struct RestState {
    pub peer_rx: watch::Receiver<Vec<String>>,
    pub control_tx: mpsc::UnboundedSender<crate::podmesh_p2p::control::Libp2pControl>,
    task_store: Arc<RwLock<HashMap<String, TaskRecord>>>,
    pub envelope_handler: std::sync::Arc<EnvelopeHandler>,
}

// Global mapping of operation_id -> manifest_cid to ensure consistent manifest ID usage across REST API and apply processing
static OPERATION_MANIFEST_MAPPING: Lazy<tokio::sync::RwLock<HashMap<String, String>>> =
    Lazy::new(|| tokio::sync::RwLock::new(HashMap::new()));

/// Store the mapping of operation_id -> manifest_cid for consistent manifest ID usage
pub async fn store_operation_manifest_mapping(operation_id: &str, manifest_cid: &str) {
    let mut map = OPERATION_MANIFEST_MAPPING.write().await;
    map.insert(operation_id.to_string(), manifest_cid.to_string());
    log::info!(
        "store_operation_manifest_mapping: operation_id={} -> manifest_cid={}",
        operation_id,
        manifest_cid
    );
}

/// Get the manifest_cid for a given operation_id
pub async fn get_manifest_cid_for_operation(operation_id: &str) -> Option<String> {
    let map = OPERATION_MANIFEST_MAPPING.read().await;
    map.get(operation_id).cloned()
}

pub fn build_router(
    peer_rx: watch::Receiver<Vec<String>>,
    control_tx: mpsc::UnboundedSender<crate::podmesh_p2p::control::Libp2pControl>,
    envelope_handler: std::sync::Arc<EnvelopeHandler>,
) -> Router {
    let task_store = Arc::new(RwLock::new(HashMap::new()));
    
    // Set the global task store reference for workload integration
    tokio::spawn({
        let task_store_clone = Arc::clone(&task_store);
        async move {
            crate::workload_integration::set_task_store(task_store_clone).await;
        }
    });
    
    let state = RestState {
        peer_rx,
        control_tx,
        task_store,
        envelope_handler,
    };
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/v1/kem_pubkey", get(get_kem_public_key))
        .route("/api/v1/signing_pubkey", get(get_signing_public_key))
        .route("/debug/dht/active_announces", get(debug_active_announces))
        .route("/debug/dht/peers", get(debug_dht_peers))
        .route("/debug/peers", get(debug_peers))
        .route("/debug/tasks", get(debug_all_tasks))
        .route(
            "/debug/workloads_by_peer/{peer_id}",
            get(debug_workloads_by_peer),
        )
        .route("/debug/local_peer_id", get(debug_local_peer_id))
        .route("/tasks/{task_id}/manifest_id", get(get_task_manifest_id))
        .route("/tasks", post(create_task))
        .route("/tasks/{task_id}", get(get_task_status))
        .route("/tasks/{task_id}", delete(delete_task))
        .route("/tasks/{task_id}/candidates", post(get_candidates))
        .route("/tasks/{task_id}/providers", post(get_task_providers))
        .route("/nodes", get(get_nodes))
        .route("/runtime/engines", get(runtime_engines))
        .route("/runtime/workloads", get(list_runtime_workloads))
        .route(
            "/runtime/workloads/{workload_id}",
            get(get_runtime_workload),
        )
        .route(
            "/runtime/workloads/{workload_id}/logs",
            get(get_runtime_workload_logs),
        )
        // Add envelope middleware to decrypt incoming requests and extract peer keys
        .layer(middleware::from_fn_with_state(
            state.envelope_handler.clone(),
            envelope_handler::envelope_middleware,
        ))
        // Passthrough routes for end-to-end encryption (no decryption by bootstrap)
        .route("/apply_direct/{peer_id}", post(apply_direct).layer(middleware::from_fn(envelope_handler::envelope_passthrough_middleware)))
        .route("/delete_direct/{peer_id}", post(delete_direct).layer(middleware::from_fn(envelope_handler::envelope_passthrough_middleware)))
        // state
        .with_state(state)
}

pub async fn get_candidates(
    Path(task_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<RestState>,
    Extension(envelope_metadata): Extension<crate::restapi::envelope_handler::EnvelopeMetadata>,
    _headers: HeaderMap,
    _body: Bytes,
) -> Result<axum::response::Response<axum::body::Body>, axum::http::StatusCode> {
    log::info!(
        "get_candidates: called for task_id={} (direct delivery mode)",
        task_id
    );

    // For direct delivery, simply query available nodes with their public keys
    let requested_replicas = params
        .get("replicas")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1);

    let request_id = format!(
        "{}:{}:{}",
        FREE_CAPACITY_PREFIX,
        task_id,
        uuid::Uuid::new_v4()
    );
    let capacity_fb = protocol::machine::build_capacity_request_with_id(
        &request_id,
        500u32,
        512u64 * 1024 * 1024,
        10u64 * 1024 * 1024 * 1024,
        requested_replicas as u32,
    );
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<String>();
    let _ = state.control_tx.send(
        crate::podmesh_p2p::control::Libp2pControl::QueryCapacityWithPayload {
            request_id: request_id.clone(),
            reply_tx: reply_tx.clone(),
            payload: capacity_fb,
        },
    );

    let mut responders: Vec<String> = Vec::new();
    let mut responder_set: HashSet<String> = HashSet::new();
    let start = std::time::Instant::now();
    let timeout = Duration::from_millis(FREE_CAPACITY_TIMEOUT_MS);
    log::info!(
        "get_candidates: waiting for capacity responses, timeout={}ms",
        FREE_CAPACITY_TIMEOUT_MS
    );

    while start.elapsed() < timeout {
        let remaining = timeout.saturating_sub(start.elapsed());
        match tokio::time::timeout(remaining, reply_rx.recv()).await {
            Ok(Some(peer)) => {
                log::info!(
                    "get_candidates: received response from peer: {}",
                    &peer[..16]
                );
                if responder_set.insert(peer.clone()) {
                    responders.push(peer);
                    // Get a few candidates to choose from
                    if responders.len() >= 5 {
                        log::info!(
                            "get_candidates: got {} candidates, that's enough",
                            responders.len()
                        );
                        break;
                    }
                }
            }
            Ok(None) => {
                log::warn!("get_candidates: channel closed");
                break;
            }
            Err(_) => {
                log::info!(
                    "get_candidates: reached timeout waiting for responses, got {} responders",
                    responders.len()
                );
                break;
            }
        }
    }

    log::info!(
        "get_candidates: finished with {} responders",
        responders.len()
    );

    // Parse candidates with their public keys
    let mut candidates: Vec<(String, String)> = Vec::new();
    for peer_with_key in &responders {
        match parse_peer_with_pubkey(peer_with_key) {
            Some((peer_id, pubkey_b64)) => {
                log::info!(
                    "get_candidates: added candidate {} with public key",
                    peer_id
                );
                candidates.push((peer_id, pubkey_b64));
            }
            None => {
                log::warn!(
                    "get_candidates: invalid candidate format '{}', skipping",
                    peer_with_key
                );
            }
        }
    }

    if candidates.is_empty() {
        log::warn!(
            "get_candidates: no eligible candidates discovered for task_id={}",
            task_id
        );
        let response_data = protocol::machine::build_candidates_response_with_keys(false, &[]);
        return create_response_for_candidates(&state, &response_data, &envelope_metadata).await;
    }

    let scheduler = Scheduler::new(SchedulerConfig {
        strategy: SchedulingStrategy::RoundRobin,
        max_candidates: Some(requested_replicas),
        enable_load_balancing: false,
    });

    let node_candidates: Vec<NodeCandidate> = candidates
        .iter()
        .map(|(peer_id, _)| NodeCandidate {
            node_id: peer_id.clone(),
            load_factor: 0.0,
            available: true,
            capabilities: NodeCapabilities::default(),
        })
        .collect();

    let scheduling_plan = match scheduler.schedule_workload(&node_candidates, requested_replicas) {
        Ok(plan) => plan,
        Err(err) => {
            log::warn!(
                "get_candidates: scheduler failed for task_id={} replicas={} err={:?}",
                task_id,
                requested_replicas,
                err
            );
            let response_data = protocol::machine::build_candidates_response_with_keys(false, &[]);
            return create_response_for_candidates(&state, &response_data, &envelope_metadata)
                .await;
        }
    };

    let mut selected_nodes: Vec<(String, String)> = Vec::new();
    for candidate_idx in scheduling_plan.selected_candidates {
        if let Some(entry) = candidates.get(candidate_idx) {
            selected_nodes.push(entry.clone());
        }
    }

    {
        let mut store = state.task_store.write().await;
        if let Some(record) = store.get_mut(&task_id) {
            record.assigned_peers = Some(
                selected_nodes
                    .iter()
                    .map(|(peer_id, _)| peer_id.clone())
                    .collect(),
            );
        }
    }

    let response_data =
        protocol::machine::build_candidates_response_with_keys(true, &selected_nodes);

    create_response_for_candidates(&state, &response_data, &envelope_metadata).await
}

async fn create_response_for_candidates(
    state: &RestState,
    response_data: &[u8],
    envelope_metadata: &crate::restapi::envelope_handler::EnvelopeMetadata,
) -> Result<axum::response::Response<axum::body::Body>, axum::http::StatusCode> {
    if !envelope_metadata.kem_pubkey.is_empty() {
        create_encrypted_response_with_key(
            &state.envelope_handler,
            response_data,
            "candidates_response",
            envelope_metadata.peer_id.as_deref(),
            &envelope_metadata.kem_pubkey,
        )
        .await
    } else {
        // No KEM key in metadata, return unencrypted response
        create_response_with_fallback(response_data).await
    }
}

/// Discover providers for a task/manifest via DHT
pub async fn get_task_providers(
    Path(task_id): Path<String>,
    State(state): State<RestState>,
    Extension(envelope_metadata): Extension<crate::restapi::envelope_handler::EnvelopeMetadata>,
) -> Result<axum::response::Response<axum::body::Body>, axum::http::StatusCode> {
    info!("get_task_providers: discovering providers for task_id={}", task_id);
    debug!("get_task_providers: envelope_metadata peer_id={:?}, has_kem_pubkey={}", envelope_metadata.peer_id, !envelope_metadata.kem_pubkey.is_empty());

    let providers = match find_manifest_providers(&task_id, &state).await {
        Ok(providers) => {
            info!("find_manifest_providers returned {} providers", providers.len());
            providers
        }
        Err(e) => {
            error!("Failed to discover providers for task_id {}: {}", task_id, e);
            let error_json = serde_json::json!({
                "providers": []
            });
            let response_data = error_json.to_string().into_bytes();
            warn!("Returning error response with 0 providers ({} bytes)", response_data.len());
            return create_response_for_providers(&state, &response_data, &envelope_metadata).await;
        }
    };

    if providers.is_empty() {
        info!("No providers found for task_id={}", task_id);
        let empty_json = serde_json::json!({
            "providers": []
        });
        let response_data = empty_json.to_string().into_bytes();
        return create_response_for_providers(&state, &response_data, &envelope_metadata).await;
    }

    // For each provider, get their signing public key (stored during peer connection)
    // For now, use a placeholder - in production, public keys should be cached from peer metadata
    let mut providers_with_keys = Vec::new();
    
    // Get local peer ID to check if we're the provider
    let (local_peer_tx, mut local_peer_rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = state.control_tx.send(
        crate::podmesh_p2p::control::Libp2pControl::GetLocalPeerId {
            reply_tx: local_peer_tx,
        },
    );
    
    let local_peer_id = match tokio::time::timeout(Duration::from_secs(1), local_peer_rx.recv()).await {
        Ok(Some(peer_id)) => Some(peer_id.to_string()),
        _ => None,
    };

    for provider_peer_id in providers {
        // If this is the local peer, get our own KEM public key for encryption
        let pubkey_b64 = if Some(&provider_peer_id) == local_peer_id.as_ref() {
            match crypto::ensure_kem_keypair_on_disk() {
                Ok((pub_bytes, _)) => base64::engine::general_purpose::STANDARD.encode(&pub_bytes),
                Err(e) => {
                    warn!("Failed to get local KEM public key: {}", e);
                    String::new()
                }
            }
        } else {
            // For remote peers, use placeholder (peer_id) until we have their real key via gossip/libp2p
            provider_peer_id.clone()
        };

        providers_with_keys.push(serde_json::json!({
            "peer_id": provider_peer_id,
            "pubkey": pubkey_b64
        }));
    }

    let response_json = serde_json::json!({
        "providers": providers_with_keys
    });

    let response_data = response_json.to_string().into_bytes();
    info!("get_task_providers: returning {} providers ({} bytes)", providers_with_keys.len(), response_data.len());
    debug!("get_task_providers: response JSON: {}", response_json);
    create_response_for_providers(&state, &response_data, &envelope_metadata).await
}

async fn create_response_for_providers(
    state: &RestState,
    response_data: &[u8],
    envelope_metadata: &crate::restapi::envelope_handler::EnvelopeMetadata,
) -> Result<axum::response::Response<axum::body::Body>, axum::http::StatusCode> {
    if !envelope_metadata.kem_pubkey.is_empty() {
        create_encrypted_response_with_key(
            &state.envelope_handler,
            response_data,
            "providers_response",
            envelope_metadata.peer_id.as_deref(),
            &envelope_metadata.kem_pubkey,
        )
        .await
    } else {
        // No KEM key in metadata, return unencrypted response
        Ok(axum::response::Response::builder()
            .header("content-type", "application/json")
            .body(axum::body::Body::from(response_data.to_vec()))
            .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?)
    }
}

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub manifest_bytes: Vec<u8>,
    pub created_at: std::time::SystemTime,
    // map of peer_id -> manifest payload for manifest distribution
    pub manifests_distributed: HashMap<String, String>,
    pub assigned_peers: Option<Vec<String>>,
    pub manifest_cid: Option<String>,
    // store last generated operation id for manifest id computation
    pub last_operation_id: Option<String>,
    pub owner_pubkey: Vec<u8>,
}

pub async fn create_task(
    State(state): State<RestState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    _headers: HeaderMap,
    Extension(envelope_metadata): Extension<crate::restapi::envelope_handler::EnvelopeMetadata>,
    body: axum::body::Bytes,
) -> Result<axum::response::Response<axum::body::Body>, axum::http::StatusCode> {
    debug!("create_task: parsing decrypted payload from envelope middleware");

    // The envelope middleware has already decrypted the payload for us
    // We receive the inner EncryptedManifest flatbuffer directly
    let payload_bytes_for_parsing = body.to_vec();

    log::info!(
        "create_task: received payload len={}, first_20_bytes={:02x?}",
        payload_bytes_for_parsing.len(),
        &payload_bytes_for_parsing[..std::cmp::min(20, payload_bytes_for_parsing.len())]
    );

    // Calculate manifest_id deterministically and get operation_id from query params
    let (manifest_id, operation_id) = if let Some(id) = params.get("manifest_id") {
        // If manifest_id is provided, use it directly (operation_id might be empty)
        (
            id.clone(),
            params
                .get("operation_id")
                .cloned()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        )
    } else if let Some(operation_id) = params.get("operation_id") {
        // Calculate manifest_id using the same method as in apply processing
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();

        // Use stable manifest name for consistent hashing
        if let Some(name) = protocol::machine::extract_manifest_name(&payload_bytes_for_parsing) {
            name.hash(&mut hasher);
        } else {
            // Fallback to content hash if no name found
            payload_bytes_for_parsing.hash(&mut hasher);
        }
        let manifest_id = format!("{:x}", hasher.finish())[..16].to_string();
        (manifest_id, operation_id.clone())
    } else {
        // Fallback: use a UUID for task_id but also generate manifest_id from content
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let operation_id = uuid::Uuid::new_v4().to_string();
        let mut hasher = DefaultHasher::new();

        // Use stable manifest name for consistent hashing
        if let Some(name) = protocol::machine::extract_manifest_name(&payload_bytes_for_parsing) {
            name.hash(&mut hasher);
        } else {
            // Fallback to content hash if no name found
            payload_bytes_for_parsing.hash(&mut hasher);
        }
        let manifest_id = format!("{:x}", hasher.finish())[..16].to_string();
        (manifest_id, operation_id)
    };

    // Use manifest_id as the task_id (since manifest_id is the central identifier)
    let task_id = manifest_id.clone();
    log::info!(
        "create_task: using manifest_id='{}' as task_id='{}'",
        manifest_id,
        task_id
    );

    // Store the operation_id -> manifest_id mapping for later self-apply lookups
    store_operation_manifest_mapping(&operation_id, &manifest_id).await;

    // Extract owner public key from secure request extensions (set by envelope middleware)
    let owner_pubkey = envelope_metadata.signing_pubkey.clone();

    log::info!(
        "create_task: owner_pubkey len={} for manifest_id={}",
        owner_pubkey.len(),
        manifest_id
    );

    // Parse as EncryptedManifest flatbuffer only (no YAML support)
    log::info!(
        "create_task: attempting to parse payload_bytes len={} as EncryptedManifest",
        payload_bytes_for_parsing.len()
    );
    log::info!(
        "create_task: payload_bytes first 20 bytes={:02x?}",
        &payload_bytes_for_parsing[..std::cmp::min(20, payload_bytes_for_parsing.len())]
    );

    // Create envelope for the encrypted payload - no longer need to parse as EncryptedManifest
    let manifest_bytes_to_store =
        if !payload_bytes_for_parsing.is_empty() && payload_bytes_for_parsing[0] == 0x02 {
            log::info!("create_task: detected encrypted manifest payload (recipient-blob format)");

            // Create a proper envelope containing the encrypted payload for decryption
            // The decryption process expects an envelope with payload_type="manifest"
            let envelope_nonce: [u8; 16] = rand::random();
            let nonce_str = base64::engine::general_purpose::STANDARD.encode(&envelope_nonce);
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);

            protocol::machine::build_envelope_canonical(
                &payload_bytes_for_parsing,
                "manifest",
                &nonce_str,
                ts,
                "ml-kem-512",
                None,
            )
        } else {
            log::info!("create_task: payload appears to be plain manifest or other format");
            payload_bytes_for_parsing
        };

    let rec = TaskRecord {
        manifest_bytes: manifest_bytes_to_store,
        created_at: std::time::SystemTime::now(),
        manifests_distributed: HashMap::new(),
        assigned_peers: None,
        manifest_cid: Some(manifest_id.clone()),
        last_operation_id: Some(operation_id),
        owner_pubkey: owner_pubkey.clone(),
    };
    {
        let mut store = state.task_store.write().await;
        log::info!(
            "create_task: storing task with task_id='{}' in task_store",
            task_id
        );
        log::info!(
            "create_task: task_store had {} tasks before insert",
            store.len()
        );
        store.insert(task_id.clone(), rec);
        log::info!(
            "create_task: task_store now has {} tasks after insert",
            store.len()
        );
        log::info!(
            "create_task: verifying task_id '{}' exists in store: {}",
            task_id,
            store.contains_key(&task_id)
        );
    }

    if !owner_pubkey.is_empty() {
        crate::workload_integration::record_manifest_owner(&manifest_id, &owner_pubkey).await;
    } else {
        log::warn!(
            "create_task: missing owner pubkey when recording manifest_id={}",
            manifest_id
        );
    }

    let response_data = protocol::machine::build_task_create_response(
        true,
        &task_id,
        &manifest_id,
        FREE_CAPACITY_TIMEOUT_MS,
    );

    // Use KEM key directly from envelope metadata for secure response encryption
    if !envelope_metadata.kem_pubkey.is_empty() {
        create_encrypted_response_with_key(
            &state.envelope_handler,
            &response_data,
            "task_create_response",
            envelope_metadata.peer_id.as_deref(),
            &envelope_metadata.kem_pubkey,
        )
        .await
    } else {
        // No KEM key in metadata, return unencrypted response
        create_response_with_fallback(&response_data).await
    }
}

// Debug: return the active announces (provider CIDs) tracked by the control module
async fn debug_active_announces(State(_state): State<RestState>) -> axum::Json<serde_json::Value> {
    // access the static ACTIVE_ANNOUNCES in control module
    let cids = crate::podmesh_p2p::control::list_active_announces();
    axum::Json(serde_json::json!({"ok": true, "cids": cids}))
}

/// Debug endpoint to get local peer ID
async fn debug_local_peer_id(State(state): State<RestState>) -> axum::Json<serde_json::Value> {
    // Get the local peer ID from the control channel
    use tokio::sync::mpsc;
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();

    let control_msg = crate::podmesh_p2p::control::Libp2pControl::GetLocalPeerId { reply_tx };

    if let Err(_) = state.control_tx.send(control_msg) {
        return axum::Json(serde_json::json!({
            "ok": false,
            "error": "Failed to send control message"
        }));
    }

    // Wait for response with timeout
    match tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv()).await {
        Ok(Some(peer_id)) => axum::Json(serde_json::json!({
            "ok": true,
            "local_peer_id": peer_id.to_string()
        })),
        Ok(None) => axum::Json(serde_json::json!({
            "ok": false,
            "error": "Control channel closed"
        })),
        Err(_) => axum::Json(serde_json::json!({
            "ok": false,
            "error": "Timeout waiting for local peer ID"
        })),
    }
}

/// Debug endpoint to get workloads deployed by a specific peer ID
async fn debug_workloads_by_peer(
    Path(peer_id): Path<String>,
    State(_state): State<RestState>,
) -> axum::Json<serde_json::Value> {
    // Try to access the global runtime registry to get MockEngine state
    #[cfg(debug_assertions)]
    {
        if let Some(registry_guard) =
            crate::workload_integration::get_global_runtime_registry().await
        {
            if let Some(ref registry) = *registry_guard {
                if let Some(mock_engine) = registry.get_engine("mock") {
                    if let Some(mock_engine) = mock_engine
                        .as_any()
                        .downcast_ref::<crate::runtime::mock::MockEngine>()
                    {
                        let peer_workloads = mock_engine.get_workloads_by_peer(&peer_id);
                        let mut workloads_json = serde_json::Map::new();

                        for workload in &peer_workloads {
                            let exported_manifest = match mock_engine
                                .export_manifest(&workload.info.id)
                                .await
                            {
                                Ok(manifest_bytes) => match String::from_utf8(manifest_bytes) {
                                    Ok(manifest_str) => Some(manifest_str),
                                    Err(e) => {
                                        log::warn!(
                                            "Failed to convert manifest bytes to string for workload {}: {}",
                                            workload.info.id,
                                            e
                                        );
                                        None
                                    }
                                },
                                Err(e) => {
                                    log::warn!(
                                        "Failed to export manifest for workload {}: {}",
                                        workload.info.id,
                                        e
                                    );
                                    None
                                }
                            };

                            workloads_json.insert(
                                workload.info.id.clone(),
                                serde_json::json!({
                                    "manifest_id": workload.info.manifest_id,
                                    "status": format!("{:?}", workload.info.status),
                                    "metadata": workload.info.metadata,
                                    "created_at": workload.info.created_at.duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default().as_secs(),
                                    "updated_at": workload.info.updated_at.duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default().as_secs(),
                                    "ports": workload.info.ports,
                                    "exported_manifest": exported_manifest,
                                })
                            );
                        }

                        return axum::Json(serde_json::json!({
                            "ok": true,
                            "peer_id": peer_id,
                            "workload_count": peer_workloads.len(),
                            "workloads": workloads_json
                        }));
                    }
                }
            }
        }
    }

    axum::Json(serde_json::json!({
        "ok": false,
        "error": "MockEngine not available",
        "peer_id": peer_id,
        "workload_count": 0,
        "workloads": {}
    }))
}

// Debug: get DHT peer information
async fn debug_dht_peers(State(state): State<RestState>) -> axum::Json<serde_json::Value> {
    use tokio::sync::mpsc;

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();

    // Send control message to get DHT peer info
    let control_msg = crate::podmesh_p2p::control::Libp2pControl::GetDhtPeers { reply_tx };

    if let Err(e) = state.control_tx.send(control_msg) {
        return axum::Json(serde_json::json!({
            "ok": false,
            "error": format!("Failed to send DHT peers request: {}", e)
        }));
    }

    // Wait for response with timeout
    match tokio::time::timeout(std::time::Duration::from_secs(5), reply_rx.recv()).await {
        Ok(Some(Ok(peer_info))) => axum::Json(serde_json::json!({
            "ok": true,
            "dht_peers": peer_info
        })),
        Ok(Some(Err(e))) => axum::Json(serde_json::json!({
            "ok": false,
            "error": format!("DHT peers query failed: {}", e)
        })),
        Ok(None) => axum::Json(serde_json::json!({
            "ok": false,
            "error": "DHT peers channel closed"
        })),
        Err(_) => axum::Json(serde_json::json!({
            "ok": false,
            "error": "DHT peers request timed out"
        })),
    }
}

async fn debug_peers(State(state): State<RestState>) -> axum::Json<serde_json::Value> {
    let peers: Vec<String> = state.peer_rx.borrow().clone();

    axum::Json(serde_json::json!({
        "ok": true,
        "peers": peers,
        "count": peers.len()
    }))
}

// Debug: list all tasks with their manifest CIDs
async fn debug_all_tasks(State(state): State<RestState>) -> axum::Json<serde_json::Value> {
    let store = state.task_store.read().await;
    let mut tasks = serde_json::Map::new();

    for (task_id, record) in store.iter() {
        tasks.insert(task_id.clone(), serde_json::json!({
            "manifest_cid": record.manifest_cid,
            "created_at": record.created_at.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
            "assigned_peers": record.assigned_peers,
            "manifest_bytes_len": record.manifest_bytes.len()
        }));
    }

    axum::Json(serde_json::json!({
        "ok": true,
        "tasks": serde_json::Value::Object(tasks)
    }))
}

async fn get_task_manifest_id(
    Path(task_id): Path<String>,
    State(state): State<RestState>,
    _headers: HeaderMap,
) -> Result<axum::response::Response<axum::body::Body>, axum::http::StatusCode> {
    let maybe = { state.task_store.read().await.get(&task_id).cloned() };
    let task = match maybe {
        Some(t) => t,
        None => {
            let error_response =
                protocol::machine::build_task_status_response("", "Error", &[], None);
            return create_response_with_fallback(&error_response).await;
        }
    };

    let operation_id = match task.last_operation_id {
        Some(o) => o,
        None => {
            let error_response =
                protocol::machine::build_task_status_response("", "Error", &[], None);
            return create_response_with_fallback(&error_response).await;
        }
    };

    // Use stored manifest_cid instead of recalculating
    let manifest_id = match get_manifest_cid_for_operation(&operation_id).await {
        Some(cid) => {
            log::info!(
                "get_task_manifest_id: returning stored manifest_cid={} for operation_id={}",
                cid,
                operation_id
            );
            cid
        }
        None => {
            // Fallback to task record manifest_cid if available
            if let Some(cid) = &task.manifest_cid {
                log::warn!(
                    "get_task_manifest_id: using task.manifest_cid={} for operation_id={}",
                    cid,
                    operation_id
                );
                cid.clone()
            } else {
                log::error!(
                    "get_task_manifest_id: no manifest_cid found for operation_id={}",
                    operation_id
                );
                let error_response =
                    protocol::machine::build_task_status_response("", "Error", &[], None);
                return create_response_with_fallback(&error_response).await;
            }
        }
    };

    let response_data = serde_json::json!({"ok": true, "manifest_id": &manifest_id});
    let response_str = serde_json::to_string(&response_data).unwrap_or_default();
    // No envelope metadata available, return unencrypted response
    create_response_with_fallback(response_str.as_bytes()).await
}

pub async fn get_task_status(
    Path(task_id): Path<String>,
    State(state): State<RestState>,
    _headers: HeaderMap,
) -> Result<axum::response::Response<axum::body::Body>, axum::http::StatusCode> {
    let maybe = { state.task_store.read().await.get(&task_id).cloned() };
    if let Some(r) = maybe {
        let assigned = r.assigned_peers.unwrap_or_default();

        let response_data = protocol::machine::build_task_status_response(
            &task_id,
            "Pending",
            &assigned,
            r.manifest_cid.as_deref(),
        );
        return create_response_with_fallback(&response_data).await;
    }
    let error_response = protocol::machine::build_task_status_response("", "Error", &[], None);
    create_response_with_fallback(&error_response).await
}

/// Delete a task by task ID - discovers providers and sends delete requests
pub async fn delete_task(
    Path(task_id): Path<String>,
    State(state): State<RestState>,
    _headers: HeaderMap,
    Extension(envelope_metadata): Extension<crate::restapi::envelope_handler::EnvelopeMetadata>,
    body: Bytes,
) -> Result<axum::response::Response<axum::body::Body>, axum::http::StatusCode> {
    info!("delete_task: task_id={}", task_id);

    // Generate operation ID for this delete request
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let operation_id = format!("delete-{}-{}", task_id, timestamp);

    // Extract requesting peer info from envelope metadata
    let origin_peer = envelope_metadata
        .peer_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    // Parse query parameters for force flag
    let force = String::from_utf8_lossy(&body).contains("force=true");

    // Step 1: Discover which nodes are providing this task/manifest
    let providers = match find_manifest_providers(&task_id, &state).await {
        Ok(providers) => providers,
        Err(e) => {
            error!(
                "Failed to discover providers for task_id {}: {}",
                task_id, e
            );
            let error_response = protocol::machine::build_delete_response(
                false,
                &operation_id,
                &format!("Failed to discover providers: {}", e),
                &task_id,
                &[],
            );
            return create_response_for_envelope_metadata(
                &state.envelope_handler,
                &error_response,
                "delete_response",
                &envelope_metadata,
            )
            .await;
        }
    };

    if providers.is_empty() {
        warn!("No providers found for task_id: {}", task_id);
        let response = protocol::machine::build_delete_response(
            true,
            &operation_id,
            "No providers found for task",
            &task_id,
            &[],
        );
        return create_response_for_envelope_metadata(
            &state.envelope_handler,
            &response,
            "delete_response",
            &envelope_metadata,
        )
        .await;
    }

    info!(
        "Found {} providers for task_id {}: {:?}",
        providers.len(),
        task_id,
        providers
    );

    // Step 2: Use the original signed envelope from the CLI for forwarding
    // This preserves the CLI's signature for verification on worker nodes
    let delete_request_to_forward = if !envelope_metadata.original_envelope.is_empty() {
        envelope_metadata.original_envelope.clone()
    } else {
        // Fallback: build a new delete request if no envelope (shouldn't happen with CLI requests)
        protocol::machine::build_delete_request(&task_id, &operation_id, &origin_peer, force)
    };

    // Step 3: Send delete requests to all providers
    let mut successful_deletes = Vec::new();
    let mut failed_deletes = Vec::new();
    let mut removed_workloads = Vec::new();

    // Get our local peer ID for comparison
    let (local_peer_tx, mut local_peer_rx) = mpsc::unbounded_channel();
    if let Err(e) =
        state
            .control_tx
            .send(crate::podmesh_p2p::control::Libp2pControl::GetLocalPeerId {
                reply_tx: local_peer_tx,
            })
    {
        error!("Failed to get local peer ID: {}", e);
        let error_response = protocol::machine::build_delete_response(
            false,
            &operation_id,
            &format!("Failed to get local peer ID: {}", e),
            &task_id,
            &[],
        );
        return create_response_for_envelope_metadata(
            &state.envelope_handler,
            &error_response,
            "delete_response",
            &envelope_metadata,
        )
        .await;
    }

    let local_peer_id =
        match tokio::time::timeout(Duration::from_secs(2), local_peer_rx.recv()).await {
            Ok(Some(peer_id)) => peer_id,
            _ => {
                error!("Timeout getting local peer ID");
                let error_response = protocol::machine::build_delete_response(
                    false,
                    &operation_id,
                    "Timeout getting local peer ID",
                    &task_id,
                    &[],
                );
                return create_response_for_envelope_metadata(
                    &state.envelope_handler,
                    &error_response,
                    "delete_response",
                    &envelope_metadata,
                )
                .await;
            }
        };

    for provider_peer_id_str in providers {
        // Parse provider peer ID
        let provider_peer_id: libp2p::PeerId = match provider_peer_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                error!("Invalid provider peer ID '{}': {}", provider_peer_id_str, e);
                failed_deletes.push(format!("{}:invalid_peer_id", provider_peer_id_str));
                continue;
            }
        };

        // Check if this is a self-delete (local peer)
        if provider_peer_id == local_peer_id {
            info!("Performing self-delete for manifest_id: {}", task_id);

            // Handle local workload deletion directly
            match handle_local_delete(&task_id, force).await {
                Ok(local_removed_workloads) => {
                    info!(
                        "Self-delete successful for manifest_id {}: removed {} workloads",
                        task_id,
                        local_removed_workloads.len()
                    );
                    successful_deletes.push(provider_peer_id_str);
                    removed_workloads.extend(local_removed_workloads);
                }
                Err(e) => {
                    error!("Self-delete failed for manifest_id {}: {}", task_id, e);
                    failed_deletes
                        .push(format!("{}:self_delete_failed:{}", provider_peer_id_str, e));
                }
            }
        } else {
            // Send delete request to remote peer
            match send_delete_request_to_peer(&state, &provider_peer_id_str, &delete_request_to_forward).await
            {
                Ok(result) => {
                    info!(
                        "Delete request sent to peer {}: {:?}",
                        provider_peer_id_str, result
                    );
                    successful_deletes.push(provider_peer_id_str);
                }
                Err(e) => {
                    error!(
                        "Failed to send delete request to peer {}: {}",
                        provider_peer_id_str, e
                    );
                    failed_deletes.push(format!("{}:{}", provider_peer_id_str, e));
                }
            }
        }
    }

    // Step 4: Return response
    let success = !successful_deletes.is_empty();
    let message = if success {
        if failed_deletes.is_empty() {
            format!(
                "Delete requests sent to {} providers",
                successful_deletes.len()
            )
        } else {
            format!(
                "Delete requests sent to {}/{} providers. Failures: {:?}",
                successful_deletes.len(),
                successful_deletes.len() + failed_deletes.len(),
                failed_deletes
            )
        }
    } else {
        format!(
            "Failed to send delete requests to any providers: {:?}",
            failed_deletes
        )
    };

    let response = protocol::machine::build_delete_response(
        success,
        &operation_id,
        &message,
        &task_id,
        &removed_workloads,
    );

    create_response_for_envelope_metadata(
        &state.envelope_handler,
        &response,
        "delete_response",
        &envelope_metadata,
    )
    .await
}

/// Handle local workload deletion when the provider is the same machine
async fn handle_local_delete(manifest_id: &str, _force: bool) -> Result<Vec<String>, String> {
    info!(
        "handle_local_delete: processing manifest_id={}",
        manifest_id
    );

    // Use the workload integration module to remove workloads by manifest ID
    match crate::workload_integration::remove_workloads_by_manifest_id(manifest_id).await {
        Ok(removed_workloads) => {
            info!(
                "handle_local_delete: successfully removed {} workloads for manifest_id '{}'",
                removed_workloads.len(),
                manifest_id
            );
            Ok(removed_workloads)
        }
        Err(e) => {
            error!(
                "handle_local_delete: failed to remove workloads for manifest_id '{}': {}",
                manifest_id, e
            );
            Err(format!("local deletion failed: {}", e))
        }
    }
}

/// Find providers for a task using cached assignment info and DHT discovery
async fn find_manifest_providers(task_id: &str, state: &RestState) -> Result<Vec<String>, String> {
    info!("find_manifest_providers: searching for task_id={}", task_id);

    // First, check if we have assigned_peers in the task record (this is faster and more reliable)
    {
        let store = state.task_store.read().await;
        if let Some(record) = store.get(task_id) {
            if let Some(ref assigned_peers) = record.assigned_peers {
                if !assigned_peers.is_empty() {
                    info!(
                        "find_manifest_providers: found {} assigned peers from task record for task_id={}",
                        assigned_peers.len(),
                        task_id
                    );
                    return Ok(assigned_peers.clone());
                }
            }
        }
    }

    // Fallback to DHT discovery if no assigned peers found
    info!("find_manifest_providers: no assigned peers found, falling back to DHT discovery");

    // Use the libp2p control system to find providers for this manifest
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    // Send request to find providers via DHT
    let control_msg = crate::podmesh_p2p::control::Libp2pControl::FindManifestHolders {
        manifest_id: task_id.to_string(),
        reply_tx: tx,
    };

    if let Err(e) = state.control_tx.send(control_msg) {
        warn!("Failed to send find providers request: {}", e);
        return Ok(vec![]);
    }

    // Wait for response with timeout
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
        Ok(Some(providers)) => {
            info!(
                "find_manifest_providers: found {} providers from DHT for task_id={}",
                providers.len(),
                task_id
            );
            Ok(providers
                .into_iter()
                .map(|peer_id| peer_id.to_string())
                .collect())
        }
        Ok(None) => {
            warn!(
                "find_manifest_providers: channel closed for task_id={}",
                task_id
            );
            Ok(vec![])
        }
        Err(_) => {
            warn!(
                "find_manifest_providers: timeout waiting for providers for task_id={}",
                task_id
            );
            Ok(vec![])
        }
    }
}

/// Send a delete request to a specific peer
async fn send_delete_request_to_peer(
    state: &RestState,
    peer_id: &str,
    delete_request: &[u8],
) -> Result<String, String> {
    info!("send_delete_request_to_peer: sending to peer={}", peer_id);

    // Parse the peer string into a PeerId
    let target_peer_id: libp2p::PeerId = peer_id
        .parse()
        .map_err(|e| format!("Invalid peer ID '{}': {}", peer_id, e))?;

    // Send the delete request via libp2p control message
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Result<String, String>>();

    let control_msg = crate::podmesh_p2p::control::Libp2pControl::SendDeleteRequest {
        peer_id: target_peer_id,
        delete_request: delete_request.to_vec(),
        reply_tx,
    };

    if let Err(e) = state.control_tx.send(control_msg) {
        return Err(format!(
            "Failed to send delete request to libp2p control: {}",
            e
        ));
    }

    // Wait for response with timeout
    match tokio::time::timeout(Duration::from_secs(10), reply_rx.recv()).await {
        Ok(Some(Ok(result))) => {
            info!(
                "Delete request to peer {} completed successfully: {}",
                peer_id, result
            );
            Ok(result)
        }
        Ok(Some(Err(e))) => {
            error!("Delete request to peer {} failed: {}", peer_id, e);
            Err(e)
        }
        Ok(None) => {
            error!("Delete request to peer {} - reply channel closed", peer_id);
            Err("Reply channel closed".to_string())
        }
        Err(_) => {
            error!("Delete request to peer {} timed out", peer_id);
            Err("Request timed out".to_string())
        }
    }
}

/// Forward an ApplyRequest directly to a specific peer via libp2p
/// This bypasses centralized task storage and forwards the request directly
pub async fn apply_direct(
    Path(peer_id): Path<String>,
    State(state): State<RestState>,
    _headers: HeaderMap,
    Extension(envelope_metadata): Extension<crate::restapi::envelope_handler::EnvelopeMetadata>,
    body: Bytes,
) -> Result<axum::response::Response<axum::body::Body>, axum::http::StatusCode> {
    debug!("apply_direct: forwarding ApplyRequest to peer {}", peer_id);

    // Use the original signed envelope from the CLI (preserved by middleware)
    // This maintains the CLI's signature for verification on the worker node
    let apply_request_bytes = if !envelope_metadata.original_envelope.is_empty() {
        envelope_metadata.original_envelope.clone()
    } else {
        // Fallback to decrypted payload if no envelope (shouldn't happen)
        body.to_vec()
    };

    // Parse the peer string into a PeerId
    let target_peer_id: libp2p::PeerId = match peer_id.parse() {
        Ok(id) => id,
        Err(e) => {
            log::warn!("apply_direct: invalid peer ID '{}': {}", peer_id, e);
            let error_response = protocol::machine::build_apply_response(
                false,
                "unknown",
                &format!("Invalid peer ID: {}", e),
            );
            return create_response_with_fallback(&error_response).await;
        }
    };

    // Forward the ApplyRequest directly to the peer via libp2p
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Result<String, String>>();
    if let Err(e) = state.control_tx.send(
        crate::podmesh_p2p::control::Libp2pControl::SendApplyRequest {
            peer_id: target_peer_id,
            manifest: apply_request_bytes,
            reply_tx,
        },
    ) {
        log::error!("apply_direct: failed to send to libp2p control: {}", e);
        let error_response = protocol::machine::build_apply_response(
            false,
            "unknown",
            "Failed to forward request to libp2p",
        );
        return create_response_with_fallback(&error_response).await;
    }

    // Wait for response with timeout
    let result = match tokio::time::timeout(Duration::from_secs(30), reply_rx.recv()).await {
        Ok(Some(Ok(_msg))) => {
            debug!("apply_direct: success for peer {}", peer_id);
            protocol::machine::build_apply_response(
                true,
                "forwarded",
                "Request forwarded successfully",
            )
        }
        Ok(Some(Err(e))) => {
            log::warn!("apply_direct: error for peer {}: {}", peer_id, e);
            protocol::machine::build_apply_response(
                false,
                "forwarded",
                &format!("Forward failed: {}", e),
            )
        }
        _ => {
            log::warn!("apply_direct: timeout for peer {}", peer_id);
            protocol::machine::build_apply_response(
                false,
                "forwarded",
                "Timeout waiting for peer response",
            )
        }
    };

    // Return response (simplified - no encryption needed for this case)
    create_response_with_fallback(&result).await
}

/// Forward a DeleteRequest directly to a specific peer via libp2p (passthrough with end-to-end encryption)
pub async fn delete_direct(
    Path(peer_id): Path<String>,
    State(state): State<RestState>,
    _headers: HeaderMap,
    Extension(envelope_metadata): Extension<crate::restapi::envelope_handler::EnvelopeMetadata>,
    body: Bytes,
) -> Result<axum::response::Response<axum::body::Body>, axum::http::StatusCode> {
    debug!("delete_direct: forwarding DeleteRequest to peer {}", peer_id);

    // Use the original signed envelope from the CLI (preserved by passthrough middleware)
    let delete_request_bytes = if !envelope_metadata.original_envelope.is_empty() {
        envelope_metadata.original_envelope.clone()
    } else {
        // Fallback to body if no envelope (shouldn't happen)
        body.to_vec()
    };

    // Parse the peer string into a PeerId
    let target_peer_id: libp2p::PeerId = match peer_id.parse() {
        Ok(id) => id,
        Err(e) => {
            log::warn!("delete_direct: invalid peer ID '{}': {}", peer_id, e);
            let error_response = protocol::machine::build_delete_response(
                false,
                "unknown",
                &format!("Invalid peer ID: {}", e),
                "unknown",
                &[],
            );
            return create_response_with_fallback(&error_response).await;
        }
    };

    // Forward the DeleteRequest directly to the peer via libp2p
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Result<String, String>>();
    if let Err(e) = state.control_tx.send(
        crate::podmesh_p2p::control::Libp2pControl::SendDeleteRequest {
            peer_id: target_peer_id,
            delete_request: delete_request_bytes,
            reply_tx,
        },
    ) {
        log::error!("delete_direct: failed to send to libp2p control: {}", e);
        let error_response = protocol::machine::build_delete_response(
            false,
            "unknown",
            "Failed to forward request to libp2p",
            "unknown",
            &[],
        );
        return create_response_with_fallback(&error_response).await;
    }

    // Wait for response with timeout
    let result = match tokio::time::timeout(Duration::from_secs(30), reply_rx.recv()).await {
        Ok(Some(Ok(_msg))) => {
            debug!("delete_direct: success for peer {}", peer_id);
            protocol::machine::build_delete_response(
                true,
                "forwarded",
                "Request forwarded successfully",
                "unknown",
                &[],
            )
        }
        Ok(Some(Err(e))) => {
            log::warn!("delete_direct: error for peer {}: {}", peer_id, e);
            protocol::machine::build_delete_response(
                false,
                "forwarded",
                &format!("Forward failed: {}", e),
                "unknown",
                &[],
            )
        }
        _ => {
            log::warn!("delete_direct: timeout for peer {}", peer_id);
            protocol::machine::build_delete_response(
                false,
                "forwarded",
                "Timeout waiting for peer response",
                "unknown",
                &[],
            )
        }
    };

    // Return response
    create_response_with_fallback(&result).await
}

#[derive(Serialize)]
struct RuntimeEngineStatus {
    name: String,
    available: bool,
    is_default: bool,
}

#[derive(Serialize)]
struct RuntimeEnginesResponse {
    default_engine: Option<String>,
    engines: Vec<RuntimeEngineStatus>,
}

#[derive(Serialize)]
struct RuntimeWorkloadsResponse {
    runtime_engine: String,
    workloads: Vec<RuntimeWorkloadView>,
}

#[derive(Serialize)]
struct RuntimeWorkloadView {
    workload_id: String,
    manifest_id: String,
    status: RuntimeWorkloadStatusView,
    runtime_engine: String,
    metadata: HashMap<String, String>,
    ports: Vec<PortMapping>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

#[derive(Serialize)]
struct RuntimeWorkloadStatusView {
    phase: String,
    message: Option<String>,
}

#[derive(Serialize)]
struct RuntimeLogResponse {
    runtime_engine: String,
    workload_id: String,
    tail: Option<usize>,
    logs: String,
}

struct EngineSelection {
    name: String,
    engine: Arc<dyn RuntimeEngine>,
}

async fn runtime_engines() -> axum::Json<RuntimeEnginesResponse> {
    let availability = crate::workload_integration::get_runtime_registry_stats().await;
    let default_engine = crate::workload_integration::get_default_runtime_engine_name().await;
    let mut engines: Vec<RuntimeEngineStatus> = availability
        .into_iter()
        .map(|(name, available)| RuntimeEngineStatus {
            is_default: default_engine
                .as_ref()
                .map(|default_name| default_name == &name)
                .unwrap_or(false),
            name,
            available,
        })
        .collect();
    engines.sort_by(|a, b| a.name.cmp(&b.name));

    axum::Json(RuntimeEnginesResponse {
        default_engine,
        engines,
    })
}

async fn list_runtime_workloads(
    Query(params): Query<HashMap<String, String>>,
) -> Result<axum::Json<RuntimeWorkloadsResponse>, StatusCode> {
    let engine_name = params.get("engine").cloned();
    let registry_guard = crate::workload_integration::get_global_runtime_registry()
        .await
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let registry = registry_guard
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let selection = select_runtime_engine(registry, engine_name.as_deref())?;

    let workloads = selection.engine.list_workloads().await.map_err(|err| {
        warn!(
            "failed to list runtime workloads for engine {}: {:?}",
            selection.name, err
        );
        runtime_error_to_status(err)
    })?;

    let response = RuntimeWorkloadsResponse {
        runtime_engine: selection.name.clone(),
        workloads: workloads
            .into_iter()
            .map(|info| RuntimeWorkloadView::from_info(info, &selection.name))
            .collect(),
    };

    Ok(axum::Json(response))
}

async fn get_runtime_workload(
    Path(workload_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<axum::Json<RuntimeWorkloadView>, StatusCode> {
    let engine_name = params.get("engine").cloned();
    let registry_guard = crate::workload_integration::get_global_runtime_registry()
        .await
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let registry = registry_guard
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let selection = select_runtime_engine(registry, engine_name.as_deref())?;

    let engine_label = selection.name.clone();
    let workload_label = workload_id.clone();
    let info = selection
        .engine
        .get_workload_status(&workload_id)
        .await
        .map_err(|err| {
            warn!(
                "failed to fetch workload state (engine={}, workload={}): {:?}",
                engine_label, workload_label, err
            );
            runtime_error_to_status(err)
        })?;

    Ok(axum::Json(RuntimeWorkloadView::from_info(
        info,
        &selection.name,
    )))
}

async fn get_runtime_workload_logs(
    Path(workload_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<axum::Json<RuntimeLogResponse>, StatusCode> {
    let engine_name = params.get("engine").cloned();
    let tail = params
        .get("tail")
        .and_then(|value| value.parse::<usize>().ok());
    let registry_guard = crate::workload_integration::get_global_runtime_registry()
        .await
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let registry = registry_guard
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let selection = select_runtime_engine(registry, engine_name.as_deref())?;

    let engine_label = selection.name.clone();
    let workload_label = workload_id.clone();
    let logs = selection
        .engine
        .get_workload_logs(&workload_id, tail)
        .await
        .map_err(|err| {
            warn!(
                "failed to fetch workload logs (engine={}, workload={}, tail={:?}): {:?}",
                engine_label, workload_label, tail, err
            );
            runtime_error_to_status(err)
        })?;

    Ok(axum::Json(RuntimeLogResponse {
        runtime_engine: selection.name.clone(),
        workload_id,
        tail,
        logs,
    }))
}

impl RuntimeWorkloadView {
    fn from_info(info: WorkloadInfo, runtime_engine: &str) -> Self {
        let WorkloadInfo {
            id,
            manifest_id,
            status,
            metadata,
            created_at,
            updated_at,
            ports,
        } = info;

        Self {
            workload_id: id,
            manifest_id,
            status: runtime_status_view(&status),
            runtime_engine: runtime_engine.to_string(),
            metadata,
            ports,
            created_at_ms: system_time_ms(created_at),
            updated_at_ms: system_time_ms(updated_at),
        }
    }
}

fn runtime_status_view(status: &WorkloadStatus) -> RuntimeWorkloadStatusView {
    let (phase, message) = match status {
        WorkloadStatus::Starting => ("starting", None),
        WorkloadStatus::Running => ("running", None),
        WorkloadStatus::Stopped => ("stopped", None),
        WorkloadStatus::Unknown => ("unknown", None),
        WorkloadStatus::Failed(reason) => ("failed", Some(reason.clone())),
    };

    RuntimeWorkloadStatusView {
        phase: phase.to_string(),
        message,
    }
}

fn system_time_ms(ts: SystemTime) -> i64 {
    match ts.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(err) => -(err.duration().as_millis() as i64),
    }
}

fn select_runtime_engine(
    registry: &crate::runtime::RuntimeRegistry,
    requested: Option<&str>,
) -> Result<EngineSelection, StatusCode> {
    if let Some(name) = requested {
        registry
            .get_engine(name)
            .map(|engine| EngineSelection {
                name: name.to_string(),
                engine,
            })
            .ok_or(StatusCode::NOT_FOUND)
    } else {
        let engine = registry
            .get_default_engine()
            .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let name = registry
            .default_engine_name()
            .unwrap_or("unknown")
            .to_string();
        Ok(EngineSelection { name, engine })
    }
}

fn runtime_error_to_status(err: RuntimeError) -> StatusCode {
    match err {
        RuntimeError::WorkloadNotFound(_) => StatusCode::NOT_FOUND,
        RuntimeError::EngineNotAvailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        RuntimeError::InvalidManifest(_) => StatusCode::BAD_REQUEST,
        RuntimeError::DeploymentFailed(_) => StatusCode::BAD_GATEWAY,
        RuntimeError::IoError(_) | RuntimeError::CommandFailed(_) => StatusCode::BAD_GATEWAY,
    }
}
