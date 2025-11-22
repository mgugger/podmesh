//! Workload Integration Module
//!
//! This module provides integration between the existing libp2p apply message handling
//! and the new workload manager system. It updates the apply message handler to use
//! the runtime engines and provider announcement system.

use crate::gateway_sidecar::{
    GATEWAY_BOOTSTRAP_ENV, GATEWAY_LOG_ENV, GATEWAY_LOG_LEVEL, GATEWAY_METADATA_ENV,
    GATEWAY_METADATA_MOUNT_PATH, GATEWAY_SIDECAR_CONTAINER_NAME, GATEWAY_VOLUME_NAME,
    GatewaySidecarSettings, gateway_sidecar_settings, metadata_container_path, metadata_host_dir,
};
use crate::podmesh_p2p::behaviour::MyBehaviour;
use crate::provider::{ProviderConfig, ProviderManager};
use crate::resource_verifier::ResourceVerifier;
use crate::runtime::{
    DeploymentConfig, GatewayInjectionConfig, RuntimeRegistry, create_default_registry,
};
use base64::Engine;
use libp2p::Swarm;
use libp2p::request_response;
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use protocol::machine;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Global runtime registry for all available engines
static RUNTIME_REGISTRY: Lazy<Arc<RwLock<Option<RuntimeRegistry>>>> =
    Lazy::new(|| Arc::new(RwLock::new(None)));

/// Global provider manager for announcements
static PROVIDER_MANAGER: Lazy<Arc<RwLock<Option<ProviderManager>>>> =
    Lazy::new(|| Arc::new(RwLock::new(None)));

/// Global resource verifier for capacity checks
static RESOURCE_VERIFIER: Lazy<Arc<ResourceVerifier>> =
    Lazy::new(|| Arc::new(ResourceVerifier::new()));

/// Node-local cache mapping manifest IDs to owner public keys.
static MANIFEST_OWNER_MAP: Lazy<RwLock<HashMap<String, Vec<u8>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Initialize the runtime registry and provider manager
pub async fn initialize_workload_manager(
    force_mock_runtime: bool,
    mock_only_runtime: bool,
    scheduling_enabled: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !scheduling_enabled {
        info!("Scheduling disabled; skipping runtime registry and provider manager initialization");
        return Ok(());
    }

    info!("Initializing runtime registry and provider manager for manifest deployment");

    // Create runtime registry - use mock-only for tests if environment variable is set
    let use_mock_registry = force_mock_runtime || mock_only_runtime;

    #[cfg(debug_assertions)]
    let registry = if use_mock_registry {
        info!("Using mock-only runtime registry for testing");
        crate::runtime::create_mock_only_registry().await
    } else {
        create_default_registry().await
    };

    #[cfg(not(debug_assertions))]
    let registry = {
        if use_mock_registry {
            warn!(
                "mock runtime requested but not compiled in release build; falling back to default registry"
            );
        }
        create_default_registry().await
    };
    let available_engines = registry.check_available_engines().await;

    info!("Available runtime engines: {:?}", available_engines);

    // Store the registry globally
    {
        let mut global_registry = RUNTIME_REGISTRY.write().await;
        *global_registry = Some(registry);
    }

    // Create provider manager
    let provider_config = ProviderConfig {
        default_ttl_seconds: 3600, // 1 hour
        ..Default::default()
    };
    let provider_manager = ProviderManager::new(provider_config);

    {
        let mut global_provider_manager = PROVIDER_MANAGER.write().await;
        *global_provider_manager = Some(provider_manager);
    }

    // Initialize resource verifier with system resources
    info!("Initializing resource verifier");
    let verifier = get_global_resource_verifier();
    if let Err(e) = verifier.update_system_resources().await {
        warn!("Failed to update system resources: {}", e);
    }

    info!("Runtime registry and provider manager initialized successfully");
    Ok(())
}

/// Record the owner public key for a manifest on this node.
pub async fn record_manifest_owner(manifest_id: &str, owner_pubkey: &[u8]) {
    let mut map = MANIFEST_OWNER_MAP.write().await;
    map.insert(manifest_id.to_string(), owner_pubkey.to_vec());
    info!(
        "record_manifest_owner: stored owner_pubkey len={} for manifest_id={}",
        owner_pubkey.len(),
        manifest_id
    );
}

/// Retrieve the owner public key for a manifest if known.
pub async fn get_manifest_owner(manifest_id: &str) -> Option<Vec<u8>> {
    let map = MANIFEST_OWNER_MAP.read().await;
    map.get(manifest_id).cloned()
}

/// Remove the owner mapping for a manifest.
pub async fn remove_manifest_owner(manifest_id: &str) -> Option<Vec<u8>> {
    let mut map = MANIFEST_OWNER_MAP.write().await;
    map.remove(manifest_id)
}

/// Get access to the global runtime registry (for testing and debug endpoints)
pub async fn get_global_runtime_registry()
-> Option<tokio::sync::RwLockReadGuard<'static, Option<RuntimeRegistry>>> {
    let registry_guard = RUNTIME_REGISTRY.read().await;
    if registry_guard.is_some() {
        Some(registry_guard)
    } else {
        None
    }
}

/// Get access to the global resource verifier
pub fn get_global_resource_verifier() -> Arc<ResourceVerifier> {
    Arc::clone(&RESOURCE_VERIFIER)
}

/// Enhanced apply message handler that uses the workload manager
pub async fn handle_apply_message_with_workload_manager(
    message: request_response::Message<Vec<u8>, Vec<u8>>,
    peer: libp2p::PeerId,
    swarm: &mut Swarm<MyBehaviour>,
    _local_peer: libp2p::PeerId,
) {
    match message {
        request_response::Message::Request {
            request, channel, ..
        } => {
            info!("Received apply request from peer={}", peer);

            // Verify request as a FlatBuffer Envelope
            let (effective_request, owner_pubkey) =
                match crate::podmesh_p2p::security::verify_envelope_and_check_nonce_for_peer(
                    &request,
                    &peer.to_string(),
                ) {
                    Ok(parts) => (parts.payload, parts.pubkey),
                    Err(e) => {
                        if crate::podmesh_p2p::security::require_signed_messages() {
                            error!("Rejecting unsigned/invalid apply request: {:?}", e);
                            let error_response = machine::build_apply_response(
                                false,
                                "unknown",
                                "unsigned or invalid envelope",
                            );
                            let _ = swarm
                                .behaviour_mut()
                                .apply_rr
                                .send_response(channel, error_response);
                            return;
                        }
                        warn!(
                            "Accepting unsigned apply request from peer={} due to relaxed policy",
                            peer
                        );
                        (request.clone(), Vec::new())
                    }
                };

            // Parse the FlatBuffer apply request
            match machine::root_as_apply_request(&effective_request) {
                Ok(apply_req) => {
                    info!(
                        "Apply request - operation_id={:?} replicas={}",
                        apply_req.operation_id(),
                        apply_req.replicas()
                    );

                    // Extract and validate manifest
                    if let Some(manifest_json) = apply_req.manifest_json() {
                        let manifest_id = apply_req.manifest_id().unwrap_or("");
                        if manifest_id.is_empty() {
                            warn!(
                                "Apply request missing manifest_id; rejecting from peer={}",
                                peer
                            );
                            let error_response = machine::build_apply_response(
                                false,
                                "unknown",
                                "missing manifest id",
                            );
                            let _ = swarm
                                .behaviour_mut()
                                .apply_rr
                                .send_response(channel, error_response);
                            return;
                        }

                        let reservation_ok = get_global_resource_verifier()
                            .has_active_reservation_for_manifest(manifest_id)
                            .await;
                        if !reservation_ok {
                            warn!(
                                "Apply request for manifest_id={} from peer={} without prior reservation",
                                manifest_id, peer
                            );
                            let error_response = machine::build_apply_response(
                                false,
                                manifest_id,
                                "no active capacity reservation",
                            );
                            let _ = swarm
                                .behaviour_mut()
                                .apply_rr
                                .send_response(channel, error_response);
                            return;
                        }

                        match process_manifest_deployment(
                            swarm,
                            &apply_req,
                            manifest_json,
                            &owner_pubkey,
                        )
                        .await
                        {
                            Ok(workload_id) => {
                                info!(
                                    "Successfully deployed workload {} for apply request",
                                    workload_id
                                );

                                // Send success response
                                let success_response = machine::build_apply_response(
                                    true,
                                    &workload_id,
                                    "workload deployed successfully",
                                );
                                let _ = swarm
                                    .behaviour_mut()
                                    .apply_rr
                                    .send_response(channel, success_response);
                            }
                            Err(e) => {
                                error!("Failed to deploy workload for apply request: {}", e);

                                // Send error response
                                let error_message = format!("deployment failed: {}", e);
                                let error_response =
                                    machine::build_apply_response(false, "unknown", &error_message);
                                let _ = swarm
                                    .behaviour_mut()
                                    .apply_rr
                                    .send_response(channel, error_response);
                            }
                        }
                    } else {
                        warn!("Apply request missing manifest JSON");
                        let error_response = machine::build_apply_response(
                            false,
                            "unknown",
                            "missing manifest JSON",
                        );
                        let _ = swarm
                            .behaviour_mut()
                            .apply_rr
                            .send_response(channel, error_response);
                    }
                }
                Err(e) => {
                    error!("Failed to parse apply request: {}", e);
                    let error_response = machine::build_apply_response(
                        false,
                        "unknown",
                        "invalid apply request format",
                    );
                    let _ = swarm
                        .behaviour_mut()
                        .apply_rr
                        .send_response(channel, error_response);
                }
            }
        }
        request_response::Message::Response { .. } => {
            debug!("Received apply response from peer={}", peer);
            // Handle response if needed
        }
    }
}

/// Process manifest deployment using the workload manager
async fn process_manifest_deployment(
    swarm: &mut Swarm<MyBehaviour>,
    apply_req: &machine::ApplyRequest<'_>,
    manifest_json: &str,
    owner_pubkey: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
    info!("Processing manifest deployment with encrypted envelope");

    // Decrypt the manifest content first
    let manifest_content = decrypt_manifest_content(manifest_json, "temp").await?;

    // Use the manifest_id from the apply request for provider announcements
    // This ensures consistency between apply and delete operations
    let manifest_id = apply_req.manifest_id().unwrap_or("unknown").to_string();

    info!(
        "Processing manifest deployment for manifest_id: {} (calculated from decrypted content)",
        manifest_id
    );

    if owner_pubkey.is_empty() {
        warn!(
            "process_manifest_deployment: missing owner pubkey for manifest_id={}",
            manifest_id
        );
    } else {
        record_manifest_owner(&manifest_id, owner_pubkey).await;
    }

    // Keep the original manifest for metadata/sidecar purposes
    let original_manifest_content = manifest_content.clone();

    let gateway_settings = gateway_sidecar_settings().await;

    // Modify manifest to set replicas=1 and inject required gateway sidecar
    // The original manifest is stored in DHT, but each node deploys with replicas=1
    let modified_manifest_content =
        prepare_manifest_for_node(&manifest_id, &manifest_content, &gateway_settings)?;

    // Create deployment configuration
    let deployment_config = create_deployment_config(
        apply_req,
        &manifest_id,
        &original_manifest_content,
        owner_pubkey,
        &gateway_settings,
    );

    // Select appropriate runtime engine based on manifest type
    let engine_name = select_runtime_engine(&modified_manifest_content).await?;
    info!(
        "Selected runtime engine '{}' for manifest_id: {}",
        engine_name, manifest_id
    );

    // Get runtime registry
    let registry_guard = RUNTIME_REGISTRY.read().await;
    let registry = registry_guard
        .as_ref()
        .ok_or("Runtime registry not initialized")?;

    // Get the selected engine
    let engine = registry
        .get_engine(&engine_name)
        .ok_or(format!("Runtime engine '{}' not available", engine_name))?;

    // Deploy the workload with modified manifest (replicas=1)
    // use peer-aware deployment for mock engine when compiled in debug builds
    #[cfg(debug_assertions)]
    let workload_info = {
        if engine_name == "mock" {
            if let Some(mock_engine) = engine
                .as_any()
                .downcast_ref::<crate::runtime::mock::MockEngine>()
            {
                debug!("Using peer-aware deployment for mock engine");
                mock_engine
                    .deploy_workload_with_peer(
                        &manifest_id,
                        &modified_manifest_content,
                        &deployment_config,
                        *swarm.local_peer_id(),
                    )
                    .await?
            } else {
                engine
                    .deploy_workload(&manifest_id, &modified_manifest_content, &deployment_config)
                    .await?
            }
        } else {
            engine
                .deploy_workload(&manifest_id, &modified_manifest_content, &deployment_config)
                .await?
        }
    };

    #[cfg(not(debug_assertions))]
    let workload_info = {
        if engine_name == "mock" {
            warn!(
                "mock runtime selected but not included in release build; proceeding with default deployment path"
            );
        }
        engine
            .deploy_workload(&manifest_id, &modified_manifest_content, &deployment_config)
            .await?
    };

    info!(
        "Workload deployed successfully: {} using engine '{}', status: {:?}",
        workload_info.id, engine_name, workload_info.status
    );

    // Announce as provider if deployment successful
    if let Some(provider_manager) = PROVIDER_MANAGER.read().await.as_ref() {
        let mut metadata = HashMap::new();
        metadata.insert("runtime_engine".to_string(), engine_name.clone());
        metadata.insert("workload_id".to_string(), workload_info.id.clone());
        metadata.insert("node_type".to_string(), "podmesh-machine".to_string());

        if let Err(e) = provider_manager.announce_provider(swarm, &manifest_id, metadata) {
            warn!(
                "Failed to announce as provider for manifest {}: {}",
                manifest_id, e
            );
        } else {
            info!(
                "Announced as provider for manifest_id: {} using engine '{}'",
                manifest_id, engine_name
            );
        }
    }

    Ok(workload_info.id)
}

/// Prepare manifest for node execution (replicas=1, gateway sidecar injection)
fn prepare_manifest_for_node(
    manifest_id: &str,
    manifest_content: &[u8],
    gateway_settings: &GatewaySidecarSettings,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut doc: serde_yaml::Value = serde_yaml::from_slice(manifest_content)
        .map_err(|e| format!("failed to parse manifest for transformation: {}", e))?;

    enforce_single_replica(&mut doc);
    inject_gateway_sidecar(&mut doc, manifest_id, gateway_settings)?;

    let modified_yaml = serde_yaml::to_string(&doc)
        .map_err(|e| format!("failed to serialize modified manifest: {}", e))?;
    info!(
        "Prepared manifest {} for single-node deployment with gateway sidecar",
        manifest_id
    );
    Ok(modified_yaml.into_bytes())
}

fn enforce_single_replica(doc: &mut serde_yaml::Value) {
    if let Some(mapping) = doc.as_mapping_mut() {
        mapping.insert(
            serde_yaml::Value::String("replicas".to_string()),
            serde_yaml::Value::Number(serde_yaml::Number::from(1)),
        );
    }

    if let Some(spec) = get_or_insert_mapping(doc, &["spec"]) {
        spec.insert(
            serde_yaml::Value::String("replicas".to_string()),
            serde_yaml::Value::Number(serde_yaml::Number::from(1)),
        );
    }
}

fn inject_gateway_sidecar(
    doc: &mut serde_yaml::Value,
    manifest_id: &str,
    gateway_settings: &GatewaySidecarSettings,
) -> Result<(), Box<dyn std::error::Error>> {
    let pod_spec = get_or_insert_pod_spec(doc).ok_or_else(|| {
        format!(
            "manifest kind missing supported pod spec for gateway injection (manifest_id={})",
            manifest_id
        )
    })?;

    let containers_key = serde_yaml::Value::String("containers".to_string());
    if !pod_spec.contains_key(&containers_key) {
        pod_spec.insert(
            containers_key.clone(),
            serde_yaml::Value::Sequence(Vec::new()),
        );
    }

    let containers_seq = pod_spec
        .get_mut(&containers_key)
        .and_then(|value| value.as_sequence_mut())
        .ok_or_else(|| "spec.containers must be a sequence".to_string())?;

    let already_present = containers_seq.iter().any(|container| {
        container
            .as_mapping()
            .and_then(|mapping| mapping.get(&serde_yaml::Value::String("name".to_string())))
            .and_then(|value| value.as_str())
            .map(|name| name == GATEWAY_SIDECAR_CONTAINER_NAME)
            .unwrap_or(false)
    });

    if already_present {
        warn!(
            "Manifest {} already declares a {} container; skipping duplicate injection",
            manifest_id, GATEWAY_SIDECAR_CONTAINER_NAME
        );
    } else {
        containers_seq.push(build_gateway_container_spec(gateway_settings));
    }

    ensure_gateway_volume(pod_spec, manifest_id)?;
    Ok(())
}

fn build_gateway_container_spec(gateway_settings: &GatewaySidecarSettings) -> serde_yaml::Value {
    let mut container = serde_yaml::Mapping::new();
    container.insert(
        serde_yaml::Value::String("name".to_string()),
        serde_yaml::Value::String(GATEWAY_SIDECAR_CONTAINER_NAME.to_string()),
    );
    container.insert(
        serde_yaml::Value::String("image".to_string()),
        serde_yaml::Value::String(gateway_settings.image.clone()),
    );
    container.insert(
        serde_yaml::Value::String("imagePullPolicy".to_string()),
        serde_yaml::Value::String("IfNotPresent".to_string()),
    );

    let mut env_entries = Vec::new();
    env_entries.push(build_env_var(
        GATEWAY_METADATA_ENV,
        &metadata_container_path(),
    ));
    env_entries.push(build_env_var(
        GATEWAY_BOOTSTRAP_ENV,
        &gateway_settings.bootstrap_peer,
    ));
    env_entries.push(build_env_var(GATEWAY_LOG_ENV, GATEWAY_LOG_LEVEL));
    container.insert(
        serde_yaml::Value::String("env".to_string()),
        serde_yaml::Value::Sequence(env_entries),
    );

    let mut mount = serde_yaml::Mapping::new();
    mount.insert(
        serde_yaml::Value::String("name".to_string()),
        serde_yaml::Value::String(GATEWAY_VOLUME_NAME.to_string()),
    );
    mount.insert(
        serde_yaml::Value::String("mountPath".to_string()),
        serde_yaml::Value::String(GATEWAY_METADATA_MOUNT_PATH.to_string()),
    );
    mount.insert(
        serde_yaml::Value::String("readOnly".to_string()),
        serde_yaml::Value::Bool(true),
    );
    container.insert(
        serde_yaml::Value::String("volumeMounts".to_string()),
        serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(mount)]),
    );

    serde_yaml::Value::Mapping(container)
}

fn build_env_var(name: &str, value: &str) -> serde_yaml::Value {
    let mut entry = serde_yaml::Mapping::new();
    entry.insert(
        serde_yaml::Value::String("name".to_string()),
        serde_yaml::Value::String(name.to_string()),
    );
    entry.insert(
        serde_yaml::Value::String("value".to_string()),
        serde_yaml::Value::String(value.to_string()),
    );
    serde_yaml::Value::Mapping(entry)
}

fn ensure_gateway_volume(
    pod_spec: &mut serde_yaml::Mapping,
    manifest_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let volumes_key = serde_yaml::Value::String("volumes".to_string());
    if !pod_spec.contains_key(&volumes_key) {
        pod_spec.insert(volumes_key.clone(), serde_yaml::Value::Sequence(Vec::new()));
    }

    let volumes_seq = pod_spec
        .get_mut(&volumes_key)
        .and_then(|value| value.as_sequence_mut())
        .ok_or_else(|| "spec.volumes must be a sequence".to_string())?;

    let volume_exists = volumes_seq.iter().any(|volume| {
        volume
            .as_mapping()
            .and_then(|mapping| mapping.get(&serde_yaml::Value::String("name".to_string())))
            .and_then(|value| value.as_str())
            .map(|name| name == GATEWAY_VOLUME_NAME)
            .unwrap_or(false)
    });

    if volume_exists {
        return Ok(());
    }

    let host_dir = metadata_host_dir(manifest_id);
    let host_dir_str = host_dir
        .to_str()
        .ok_or_else(|| format!("invalid metadata host directory for {}", manifest_id))?
        .to_string();

    let mut host_path = serde_yaml::Mapping::new();
    host_path.insert(
        serde_yaml::Value::String("path".to_string()),
        serde_yaml::Value::String(host_dir_str),
    );
    host_path.insert(
        serde_yaml::Value::String("type".to_string()),
        serde_yaml::Value::String("DirectoryOrCreate".to_string()),
    );

    let mut volume_entry = serde_yaml::Mapping::new();
    volume_entry.insert(
        serde_yaml::Value::String("name".to_string()),
        serde_yaml::Value::String(GATEWAY_VOLUME_NAME.to_string()),
    );
    volume_entry.insert(
        serde_yaml::Value::String("hostPath".to_string()),
        serde_yaml::Value::Mapping(host_path),
    );

    volumes_seq.push(serde_yaml::Value::Mapping(volume_entry));
    Ok(())
}

fn get_or_insert_pod_spec(doc: &mut serde_yaml::Value) -> Option<&mut serde_yaml::Mapping> {
    let kind_value = doc
        .as_mapping()
        .and_then(|mapping| mapping.get(&serde_yaml::Value::String("kind".to_string())))
        .and_then(|value| value.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "Pod".to_string());

    match kind_value.as_str() {
        "Pod" => get_or_insert_mapping(doc, &["spec"]),
        "Deployment" | "ReplicaSet" | "DaemonSet" | "StatefulSet" => {
            get_or_insert_mapping(doc, &["spec", "template", "spec"])
        }
        _ => None,
    }
}

fn get_or_insert_mapping<'a>(
    root: &'a mut serde_yaml::Value,
    path: &[&str],
) -> Option<&'a mut serde_yaml::Mapping> {
    let mut current = root;

    for key in path.iter() {
        if !current.is_mapping() {
            *current = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        }

        let mapping = current.as_mapping_mut()?;
        let yaml_key = serde_yaml::Value::String((*key).to_string());

        let needs_replacement = match mapping.get(&yaml_key) {
            Some(existing) => !existing.is_mapping(),
            None => true,
        };

        if needs_replacement {
            mapping.insert(
                yaml_key.clone(),
                serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
            );
        }

        current = mapping.get_mut(&yaml_key).unwrap();
    }

    current.as_mapping_mut()
}

/// Select the appropriate runtime engine based on manifest content and annotations
async fn select_runtime_engine(
    manifest_content: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
    let manifest_str = String::from_utf8_lossy(manifest_content);

    // Try to parse as YAML and look for annotations
    if let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&manifest_str) {
        // Check for runtime engine annotation
        if let Some(metadata) = doc.get("metadata") {
            if let Some(annotations) = metadata.get("annotations") {
                if let Some(engine) = annotations
                    .get("podmesh.io/runtime-engine")
                    .and_then(|v| v.as_str())
                {
                    info!("Found runtime engine annotation: {}", engine);
                    return Ok(engine.to_string());
                }
            }
        }

        // Check manifest type - note preferences, but don't hardcode
        if let Some(kind) = doc.get("kind").and_then(|k| k.as_str()) {
            match kind {
                "Pod" | "Deployment" | "Service" | "ConfigMap" | "Secret" => {
                    // Kubernetes resources - will prefer Podman below if available
                    debug!("Detected Kubernetes manifest kind: {}", kind);
                }
                _ => {}
            }
        }

        // Check for Docker Compose format and warn because only Podman is supported
        if doc.get("services").is_some() && doc.get("version").is_some() {
            warn!(
                "Detected Docker Compose manifest but only Podman runtime is supported; attempting deployment via Podman"
            );
        }
    }

    // Get available engines and select the best one
    if let Some(registry) = RUNTIME_REGISTRY.read().await.as_ref() {
        let available = registry.check_available_engines().await;

        // Prefer Podman, then mock for testing
        if *available.get("podman").unwrap_or(&false) {
            return Ok("podman".to_string());
        } else if *available.get("mock").unwrap_or(&false) {
            return Ok("mock".to_string());
        }
    }

    Err("No suitable runtime engine available".into())
}

/// Decrypt manifest content from the encrypted envelope
async fn decrypt_manifest_content(
    manifest_json: &str,
    manifest_id: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    debug!(
        "Decrypting manifest content for manifest_id: {}",
        manifest_id
    );

    // Decode base64-encoded envelope
    let envelope_bytes = base64::engine::general_purpose::STANDARD.decode(manifest_json)?;

    // Parse as flatbuffer envelope
    let envelope = machine::root_as_envelope(&envelope_bytes)?;

    let payload_type = envelope.payload_type().unwrap_or("");
    debug!("Envelope payload type: {}", payload_type);

    if payload_type == "manifest" {
        // Extract encrypted payload from envelope and decrypt directly
        if let Some(payload_vector) = envelope.payload() {
            let payload_bytes = payload_vector.bytes();

            // Attempt to decrypt the manifest using KEM decryption
            match decrypt_manifest_from_envelope(manifest_id, payload_bytes).await {
                Ok(decrypted_content) => {
                    info!(
                        "Successfully decrypted manifest for manifest_id: {}",
                        manifest_id
                    );
                    Ok(decrypted_content.into_bytes())
                }
                Err(e) => {
                    error!(
                        "Failed to decrypt manifest for manifest_id {}: {}",
                        manifest_id, e
                    );
                    Err(format!("decryption failed: {}", e).into())
                }
            }
        } else {
            Err("Missing payload in envelope".into())
        }
    } else {
        // Assume it's plain YAML/JSON manifest
        debug!("Treating as plain manifest content");
        Ok(manifest_json.as_bytes().to_vec())
    }
}

/// Create deployment configuration from apply request
fn create_deployment_config(
    apply_req: &machine::ApplyRequest,
    manifest_id: &str,
    original_manifest: &[u8],
    owner_pubkey: &[u8],
    gateway_settings: &GatewaySidecarSettings,
) -> DeploymentConfig {
    let mut config = DeploymentConfig::default();

    // Set replicas
    config.replicas = apply_req.replicas();

    // Metadata from apply request can be added here if needed
    if let Some(operation_id) = apply_req.operation_id() {
        config
            .env
            .insert("PODMESH_OPERATION_ID".to_string(), operation_id.to_string());
    }

    config.gateway = Some(GatewayInjectionConfig {
        image: gateway_settings.image.clone(),
        bootstrap_peer: gateway_settings.bootstrap_peer.clone(),
        manifest_id: manifest_id.to_string(),
        manifest_bytes: original_manifest.to_vec(),
        owner_public_key: owner_pubkey.to_vec(),
    });

    config
}

/// Decrypt manifest directly from envelope payload using KEM
async fn decrypt_manifest_from_envelope(
    manifest_id: &str,
    payload_bytes: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
    debug!(
        "Attempting to decrypt manifest from envelope payload for manifest_id: {}",
        manifest_id
    );

    // Validate recipient-blob version byte
    if payload_bytes.is_empty() || payload_bytes[0] != 0x02 {
        return Err(
            "unsupported payload format: expected recipient-blob (version byte 0x02)".into(),
        );
    }

    // Use the node's KEM private key to decapsulate and decrypt the recipient-blob
    let (_pub_bytes, priv_bytes) = crypto::ensure_kem_keypair_on_disk()
        .map_err(|e| format!("failed to load KEM keypair: {}", e))?;

    let plaintext = crypto::decrypt_payload_from_recipient_blob(payload_bytes, &priv_bytes)
        .map_err(|e| format!("recipient-blob decryption failed: {}", e))?;

    // Interpret plaintext as UTF-8 manifest content (YAML/JSON)
    let manifest_str = String::from_utf8(plaintext)
        .map_err(|e| format!("decrypted manifest is not valid UTF-8: {}", e))?;

    debug!(
        "Successfully decrypted manifest from envelope (len={})",
        manifest_str.len()
    );

    Ok(manifest_str)
}

/// Enhanced self-apply processing with workload manager
pub async fn process_enhanced_self_apply_request(manifest: &[u8], swarm: &mut Swarm<MyBehaviour>) {
    debug!(
        "Processing enhanced self-apply request (manifest len={})",
        manifest.len()
    );

    match machine::root_as_apply_request(manifest) {
        Ok(apply_req) => {
            debug!(
                "Enhanced self-apply request - operation_id={:?} replicas={}",
                apply_req.operation_id(),
                apply_req.replicas()
            );

            if let Some(manifest_json) = apply_req.manifest_json() {
                let owner_pubkey =
                    crypto::keypair_manager::KeypairManager::get_default_signing_keypair()
                        .map(|(pub_bytes, _)| pub_bytes)
                        .unwrap_or_default();

                match process_manifest_deployment(swarm, &apply_req, manifest_json, &owner_pubkey)
                    .await
                {
                    Ok(workload_id) => {
                        info!(
                            "Successfully deployed self-applied workload: {}",
                            workload_id
                        );
                    }
                    Err(e) => {
                        error!("Failed to deploy self-applied workload: {}", e);
                    }
                }
            } else {
                warn!("Self-apply request missing manifest JSON");
            }
        }
        Err(e) => {
            error!("Failed to parse self-apply request: {}", e);
        }
    }
}

/// Get runtime registry statistics
pub async fn get_runtime_registry_stats() -> HashMap<String, bool> {
    if let Some(registry) = RUNTIME_REGISTRY.read().await.as_ref() {
        registry.check_available_engines().await
    } else {
        HashMap::new()
    }
}

/// List all available runtime engines
pub async fn list_available_engines() -> Vec<String> {
    if let Some(registry) = RUNTIME_REGISTRY.read().await.as_ref() {
        registry
            .list_engines()
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        Vec::new()
    }
}

/// Fetch the name of the default runtime engine, if configured
pub async fn get_default_runtime_engine_name() -> Option<String> {
    if let Some(registry_guard) = get_global_runtime_registry().await {
        registry_guard
            .as_ref()
            .and_then(|registry| registry.default_engine_name())
            .map(|name| name.to_string())
    } else {
        None
    }
}

/// Remove a workload by ID (requires engine name)
pub async fn remove_workload_by_id(
    workload_id: &str,
    engine_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry_guard = RUNTIME_REGISTRY.read().await;
    let registry = registry_guard
        .as_ref()
        .ok_or("Runtime registry not initialized")?;

    let engine = registry
        .get_engine(engine_name)
        .ok_or(format!("Runtime engine '{}' not available", engine_name))?;

    engine.remove_workload(workload_id).await?;
    info!(
        "Successfully removed workload: {} from engine: {}",
        workload_id, engine_name
    );
    Ok(())
}

/// Remove workloads by manifest ID - searches through all engines and removes matching workloads
pub async fn remove_workloads_by_manifest_id(
    manifest_id: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    info!(
        "remove_workloads_by_manifest_id: manifest_id={}",
        manifest_id
    );

    let registry_guard = RUNTIME_REGISTRY.read().await;
    let registry = registry_guard
        .as_ref()
        .ok_or("Runtime registry not initialized")?;

    let mut removed_workloads = Vec::new();
    let mut errors = Vec::new();
    let engine_names = registry.list_engines();

    for engine_name in &engine_names {
        if let Some(engine) = registry.get_engine(engine_name) {
            info!(
                "Checking engine '{}' for workloads with manifest_id '{}'",
                engine_name, manifest_id
            );

            // List workloads from this engine
            match engine.list_workloads().await {
                Ok(workloads) => {
                    // Find workloads that match the manifest_id
                    for workload in workloads {
                        if workload.manifest_id == manifest_id {
                            info!(
                                "Found matching workload: {} in engine '{}'",
                                workload.id, engine_name
                            );

                            // Remove the workload
                            match engine.remove_workload(&workload.id).await {
                                Ok(()) => {
                                    info!(
                                        "Successfully removed workload: {} from engine '{}'",
                                        workload.id, engine_name
                                    );
                                    removed_workloads.push(workload.id);
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to remove workload {} from engine '{}': {}",
                                        workload.id, engine_name, e
                                    );
                                    errors.push(format!("{}:{}", engine_name, workload.id));
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to list workloads from engine '{}': {}",
                        engine_name, e
                    );
                    errors.push(format!("list:{}", engine_name));
                }
            }
        }
    }

    // Also withdraw provider announcement if we were providing this manifest
    if let Some(provider_manager) = PROVIDER_MANAGER.read().await.as_ref() {
        if let Err(e) = provider_manager.stop_providing(manifest_id) {
            warn!(
                "Failed to stop providing manifest_id {}: {}",
                manifest_id, e
            );
        } else {
            info!("Stopped providing manifest_id: {}", manifest_id);
        }
    }

    if errors.is_empty() {
        // Drop the cached owner once the manifest workloads are removed successfully.
        if remove_manifest_owner(manifest_id).await.is_some() {
            info!(
                "remove_workloads_by_manifest_id: cleared owner mapping for manifest_id={}",
                manifest_id
            );
        }
    } else {
        warn!(
            "remove_workloads_by_manifest_id: retaining owner mapping for manifest_id={} due to errors {:?}",
            manifest_id, errors
        );
    }

    info!(
        "remove_workloads_by_manifest_id completed: removed {} workloads for manifest_id '{}'",
        removed_workloads.len(),
        manifest_id
    );
    Ok(removed_workloads)
}

/// Get logs from a workload (requires engine name)
pub async fn get_workload_logs_by_id(
    workload_id: &str,
    engine_name: &str,
    tail: Option<usize>,
) -> Result<String, Box<dyn std::error::Error>> {
    let registry_guard = RUNTIME_REGISTRY.read().await;
    let registry = registry_guard
        .as_ref()
        .ok_or("Runtime registry not initialized")?;

    let engine = registry
        .get_engine(engine_name)
        .ok_or(format!("Runtime engine '{}' not available", engine_name))?;

    let logs = engine.get_workload_logs(workload_id, tail).await?;
    Ok(logs)
}

/// Discover providers for a manifest
pub async fn discover_manifest_providers(
    swarm: &mut Swarm<MyBehaviour>,
    manifest_id: &str,
) -> Result<Vec<crate::provider::ProviderInfo>, Box<dyn std::error::Error>> {
    if let Some(provider_manager) = PROVIDER_MANAGER.read().await.as_ref() {
        let providers = provider_manager
            .discover_providers(swarm, manifest_id)
            .await?;
        Ok(providers)
    } else {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_runtime_registry_initialization() {
        let result = initialize_workload_manager(false, false, true).await;
        assert!(result.is_ok());

        let stats = get_runtime_registry_stats().await;
        assert!(!stats.is_empty());

        let engines = list_available_engines().await;
        assert!(!engines.is_empty());
        assert!(engines.contains(&"mock".to_string()));
    }

    #[tokio::test]
    async fn test_runtime_engine_selection() {
        // Initialize registry for testing
        let _ = initialize_workload_manager(false, false, true).await;

        // Test Kubernetes manifest
        let k8s_manifest = r#"
apiVersion: v1
kind: Pod
metadata:
  name: test-pod
spec:
  containers:
  - name: nginx
    image: nginx:latest
"#;
        let engine = select_runtime_engine(k8s_manifest.as_bytes()).await;
        assert!(engine.is_ok());
        // Should select mock engine in test environment
        let engine_name = engine.unwrap();
        assert!(engine_name == "mock" || engine_name == "podman");

                // Test Docker Compose manifest (should still fall back to Podman/mock)
                let compose_manifest = r#"
version: '3.8'
services:
  web:
    image: nginx:latest
    ports:
      - "80:80"
"#;
                let engine = select_runtime_engine(compose_manifest.as_bytes()).await;
                assert!(engine.is_ok());
                let compose_engine = engine.unwrap();
                assert!(compose_engine == "mock" || compose_engine == "podman");
    }

    #[test]
    fn test_deployment_config_creation() {
        // This would require creating a mock ApplyRequest
        let config = DeploymentConfig::default();
        assert_eq!(config.replicas, 1);
        assert!(config.env.is_empty());
    }

    #[test]
    fn test_prepare_manifest_injects_gateway_sidecar() {
        let manifest = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: demo
spec:
  replicas: 3
  template:
    metadata:
      labels:
        app: demo
    spec:
      containers:
      - name: web
        image: nginx:latest
"#;

        let settings = GatewaySidecarSettings {
            image: "podmesh/sidecar:test".to_string(),
            bootstrap_peer: "/ip4/10.0.0.1/udp/7001/quic-v1".to_string(),
        };

        let modified = prepare_manifest_for_node("demo", manifest.as_bytes(), &settings)
            .expect("manifest transforms");
        let doc: serde_yaml::Value = serde_yaml::from_slice(&modified).expect("yaml parse");

        let containers = doc
            .get("spec")
            .and_then(|spec| spec.get("template"))
            .and_then(|template| template.get("spec"))
            .and_then(|spec| spec.get("containers"))
            .and_then(|value| value.as_sequence())
            .expect("containers sequence present");

        assert!(containers.iter().any(|container| {
            container.get("name").and_then(|value| value.as_str())
                == Some(crate::gateway_sidecar::GATEWAY_SIDECAR_CONTAINER_NAME)
        }));

        let volumes = doc
            .get("spec")
            .and_then(|spec| spec.get("template"))
            .and_then(|template| template.get("spec"))
            .and_then(|spec| spec.get("volumes"))
            .and_then(|value| value.as_sequence())
            .expect("volumes sequence present");

        assert!(volumes.iter().any(|volume| {
            volume.get("name").and_then(|value| value.as_str())
                == Some(crate::gateway_sidecar::GATEWAY_VOLUME_NAME)
        }));

        let volume_entry = volumes
            .iter()
            .find(|volume| {
                volume.get("name").and_then(|value| value.as_str())
                    == Some(crate::gateway_sidecar::GATEWAY_VOLUME_NAME)
            })
            .expect("gateway volume present");

        let host_path = volume_entry
            .get("hostPath")
            .and_then(|value| value.get("path"))
            .and_then(|value| value.as_str())
            .expect("hostPath path");
        assert!(host_path.contains("/var/lib/podmesh/sidecar/demo"));
    }
}
