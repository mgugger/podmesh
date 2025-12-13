use crypto::{encrypt_payload_for_recipient, ensure_keypair_on_disk};
use log::debug;
use log::error;
use log::info;
use protocol::machine::parse_peer_with_pubkey;
use protocol::manifest_policy::validate_and_mutate_manifest;
use protocol::manifest_yaml::parse_manifest_to_json;
use uuid::Uuid;
use std::env;

use serde_json::Value as JsonValue;
use std::path::PathBuf;

mod api_client;
use api_client::ApiClient;

fn resolve_api_base(override_url: Option<&str>) -> String {
    override_url
        .map(str::to_string)
        .or_else(|| env::var("PODMESH_API").ok())
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}

fn extract_manifest_name_from_json(manifest_json: &serde_json::Value) -> Option<String> {
    match manifest_json {
        serde_json::Value::Object(_) => manifest_json
            .get("metadata")?
            .get("name")?
            .as_str()
            .map(|s| s.to_string()),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(extract_manifest_name_from_json)
            .next(),
        _ => None,
    }
}

fn compute_manifest_id_for_owner(
    manifest_json: &JsonValue,
    owner_pubkey: &[u8],
) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};

    let manifest_name = extract_manifest_name_from_json(manifest_json).ok_or_else(|| {
        anyhow::anyhow!("Manifest must have a name field in metadata for deployment")
    })?;

    // Use SHA-256 for cryptographically secure manifest ID generation.
    // Hash format: SHA256(owner_pubkey || manifest_name)
    let mut hasher = Sha256::new();
    hasher.update(owner_pubkey);
    hasher.update(manifest_name.as_bytes());
    let result = hasher.finalize();
    // Use first 16 bytes (128 bits) encoded as hex for the manifest ID
    // Format as lowercase hex string manually to avoid hex dependency
    let hex_chars: String = result[..16]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    Ok(hex_chars)
}

fn parse_selected_nodes(peers: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    peers
        .iter()
        .map(|entry| {
            parse_peer_with_pubkey(entry).ok_or_else(|| {
                anyhow::anyhow!(
                    "Invalid candidate format: expected 'peer_id:pubkey_b64', got '{}'",
                    entry
                )
            })
        })
        .collect()
}

pub async fn apply_file(path: PathBuf, api_base: Option<&str>) -> anyhow::Result<String> {
    debug!("apply_file called for path: {:?}", path);

    if !path.exists() {
        error!("apply_file: file not found: {}", path.display());
        anyhow::bail!("file not found: {}", path.display());
    }

    let contents = tokio::fs::read_to_string(&path).await?;
    debug!(
        "apply_file: file contents read successfully, length: {}",
        contents.len()
    );
    info!(
        "File contents read successfully, length: {}",
        contents.len()
    );

    // Validate manifest against policies and apply mutations (inject defaults)
    let validated_contents = match validate_and_mutate_manifest(&contents) {
        Ok(mutated) => {
            info!("Manifest passed policy validation");
            mutated
        }
        Err(e) => {
            error!("Manifest failed policy validation: {}", e);
            anyhow::bail!("Policy validation failed: {}", e);
        }
    };

    // Parse validated manifest to JSON
    let manifest_json =
        parse_manifest_to_json(&validated_contents).unwrap_or_else(|_| serde_json::json!({"raw": validated_contents}));
    debug!("apply_file: manifest parsed successfully");

    // Extract replicas count from manifest (check spec.replicas or top-level replicas, default to 1)
    let replicas = manifest_json
        .get("spec")
        .and_then(|s| s.get("replicas"))
        .and_then(|r| r.as_u64())
        .or_else(|| manifest_json.get("replicas").and_then(|r| r.as_u64()))
        .unwrap_or(1) as usize;

    info!("Manifest requires {} replicas", replicas);

    // Ensure CLI keypair - always use persistent keypairs for consistency
    let (pk_bytes, _sk_bytes) = ensure_keypair_on_disk()?;

    let manifest_id = compute_manifest_id_for_owner(&manifest_json, &pk_bytes)?;
    if let Some(name) = extract_manifest_name_from_json(&manifest_json) {
        debug!("CLI: Using manifest name '{}' for manifest_id", name);
    }
    debug!(
        "CLI: Computed manifest_id: {} with pubkey: {:02x?}",
        manifest_id,
        &pk_bytes[..8]
    );

    let base = resolve_api_base(api_base);
    debug!("Creating ApiClient with base URL: {}", base);
    let mut api_client = ApiClient::new(base)?;

    debug!("Fetching machine's public key...");
    api_client.fetch_machine_public_key().await?;
    debug!("Successfully fetched machine's public key");

    // 1) Get candidates for node selection
    debug!("About to call get_candidates...");
    let peers = api_client.get_candidates(&manifest_id, replicas).await?;
    debug!(
        "apply_file: get_candidates completed successfully, found {} peers",
        peers.len()
    );

    if peers.is_empty() {
        anyhow::bail!("Machine returned no nodes for scheduling");
    }

    if peers.len() < replicas {
        anyhow::bail!(
            "Machine returned insufficient nodes: need {}, got {}",
            replicas,
            peers.len()
        );
    }

    let mut selected_nodes = parse_selected_nodes(&peers)?;
    if selected_nodes.len() > replicas {
        selected_nodes.truncate(replicas);
    }

    if selected_nodes.len() < replicas {
        anyhow::bail!(
            "Machine returned invalid node assignments (expected {} entries)",
            replicas
        );
    }

    info!(
        "Scheduled {} nodes for {} replicas via machine selection: {:?}",
        selected_nodes.len(),
        replicas,
        selected_nodes.iter().map(|(id, _)| id).collect::<Vec<_>>()
    );
    debug!("Selected nodes with pubkeys: {:?}", selected_nodes);

    let ts = p2p::timestamp_millis();
    // Use validated (and possibly mutated) manifest for deployment
    let manifest_to_deploy = validated_contents;
    let mut succeeded_nodes = Vec::new();
    let mut failed_nodes: Vec<(String, String)> = Vec::new();

    for (node_id, node_pubkey) in &selected_nodes {
        match send_apply_to_node(
            &api_client,
            node_id,
            node_pubkey,
            &manifest_to_deploy,
            &manifest_id,
            ts,
        )
        .await
        {
            Ok(_) => {
                succeeded_nodes.push(node_id.clone());
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
            Err(err) => {
                let err_msg = err.to_string();
                log::error!("Direct apply failed for node {}: {}", node_id, err_msg);
                failed_nodes.push((node_id.to_string(), err_msg));
            }
        }
    }

    if failed_nodes.is_empty() {
        info!(
            "Apply completed for manifest_id {} distributed to {} nodes (all will announce same ID to DHT)",
            manifest_id,
            succeeded_nodes.len()
        );
        return Ok(manifest_id);
    }

    if !succeeded_nodes.is_empty() {
        log::warn!(
            "Apply partially succeeded for manifest {} ({} ok / {} failed); some nodes already received the workload",
            manifest_id,
            succeeded_nodes.len(),
            failed_nodes.len()
        );
    }

    let failure_count = failed_nodes.len();
    let failure_summary = failed_nodes
        .into_iter()
        .map(|(node, err)| format!("{node}: {err}"))
        .collect::<Vec<_>>()
        .join("; ");

    anyhow::bail!(
        "Apply failed for {} of {} target nodes: {}",
        failure_count,
        selected_nodes.len(),
        failure_summary
    );
}

async fn send_apply_to_node(
    api_client: &ApiClient,
    node_id: &str,
    node_pubkey_b64: &str,
    manifest_payload: &str,
    manifest_id: &str,
    timestamp_ms: u64,
) -> anyhow::Result<()> {
    debug!("Creating encrypted task for node: {}", node_id);

    let node_pubkey_bytes = crypto::b64_decode(node_pubkey_b64)
        .map_err(|e| anyhow::anyhow!("Failed to decode node public key for {}: {}", node_id, e))?;

    let encrypted_blob =
        encrypt_payload_for_recipient(&node_pubkey_bytes, manifest_payload.as_bytes())?;

    let encrypted_manifest_bytes = protocol::machine::build_envelope_canonical_with_peer(
        &encrypted_blob,
        "manifest",
        "",
        timestamp_ms,
        "x25519",
        "",
        None,
    );

    debug!(
        "Sending ApplyRequest directly to node {} with manifest_id {}",
        node_id, manifest_id
    );

    let operation_id = Uuid::new_v4().to_string();
    let manifest_json_b64 = crypto::b64_encode(&encrypted_manifest_bytes);

    let apply_request_bytes = protocol::machine::build_apply_request(
        1,
        &operation_id,
        &manifest_json_b64,
        "",
        manifest_id,
    );

    let url = format!(
        "{}/apply_direct/{}",
        api_client.base_url.trim_end_matches('/'),
        node_id
    );

    // Encrypt and sign the ApplyRequest for the target worker node (end-to-end encryption)
    // The bootstrap node will route based on URL path without decrypting the payload
    let response_bytes = api_client
        .send_encrypted_request_to_node(&url, &apply_request_bytes, "apply_request", &node_pubkey_bytes)
        .await?;

    let apply_response = protocol::machine::root_as_apply_response(&response_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to parse ApplyResponse: {}", e))?;

    if !apply_response.ok() {
        anyhow::bail!(
            "Direct apply failed for node {}: {}",
            node_id,
            apply_response.message().unwrap_or("unknown error")
        );
    }

    debug!("Direct apply successful for node {}", node_id);
    Ok(())
}

pub async fn delete_file(
    path: PathBuf,
    force: bool,
    api_base: Option<&str>,
) -> anyhow::Result<String> {
    debug!("delete_file called for path: {:?}, force: {}", path, force);

    if !path.exists() {
        error!("delete_file: file not found: {}", path.display());
        anyhow::bail!("file not found: {}", path.display());
    }

    let contents = tokio::fs::read_to_string(&path).await?;
    debug!(
        "delete_file: file contents read successfully, length: {}",
        contents.len()
    );
    info!(
        "File contents read successfully, length: {}",
        contents.len()
    );

    // Parse manifest to JSON if possible
    let manifest_json =
        parse_manifest_to_json(&contents).unwrap_or_else(|_| serde_json::json!({"raw": contents}));
    debug!("delete_file: manifest parsed successfully");

    // Ensure CLI keypair - always use persistent keypairs for consistency
    let (pk_bytes, _sk_bytes) = ensure_keypair_on_disk()?;

    let manifest_id = compute_manifest_id_for_owner(&manifest_json, &pk_bytes)?;
    if let Some(name) = extract_manifest_name_from_json(&manifest_json) {
        debug!("CLI: Using manifest name '{}' for manifest_id", name);
    }
    debug!(
        "CLI: Computed manifest_id for deletion: {} with pubkey: {:02x?}",
        manifest_id,
        &pk_bytes[..8]
    );

    // API base URL can be overridden with PODMESH_API env var
    let base = api_base
        .map(|s| s.to_string())
        .or_else(|| env::var("PODMESH_API").ok())
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());
    debug!("Creating ApiClient with base URL: {}", base);
    let mut api_client = ApiClient::new(base)?;

    // Fetch machine's public key for encrypted communication
    debug!("Fetching machine's public key...");
    api_client.fetch_machine_public_key().await?;
    debug!("Successfully fetched machine's public key");

    // Step 1: Discover which nodes are providing this manifest via DHT
    debug!("Discovering providers for manifest_id: {}", manifest_id);
    let providers = discover_manifest_providers(&api_client, &manifest_id).await?;

    if providers.is_empty() {
        info!("No providers found for manifest_id: {}", manifest_id);
        return Ok(manifest_id);
    }

    info!(
        "Found {} providers for manifest_id {}: {:?}",
        providers.len(),
        manifest_id,
        providers.iter().map(|(id, _)| id).collect::<Vec<_>>()
    );

    // Step 2: Send delete requests directly to each provider node (end-to-end encryption)
    let operation_id = Uuid::new_v4().to_string();
    let mut succeeded_nodes = Vec::new();
    let mut failed_nodes: Vec<(String, String)> = Vec::new();

    for (node_id, node_pubkey_b64) in &providers {
        match send_delete_to_node(
            &api_client,
            node_id,
            node_pubkey_b64,
            &manifest_id,
            &operation_id,
            force,
        )
        .await
        {
            Ok(_) => {
                succeeded_nodes.push(node_id.clone());
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
            Err(err) => {
                let err_msg = err.to_string();
                log::error!("Direct delete failed for node {}: {}", node_id, err_msg);
                failed_nodes.push((node_id.clone(), err_msg));
            }
        }
    }

    if failed_nodes.is_empty() {
        info!(
            "Delete completed for manifest_id {} on {} nodes",
            manifest_id,
            succeeded_nodes.len()
        );
        return Ok(manifest_id);
    }

    if !succeeded_nodes.is_empty() {
        log::warn!(
            "Delete partially succeeded for manifest {} ({} ok / {} failed)",
            manifest_id,
            succeeded_nodes.len(),
            failed_nodes.len()
        );
    }

    let failure_count = failed_nodes.len();
    let failure_summary = failed_nodes
        .into_iter()
        .map(|(node, err)| format!("{node}: {err}"))
        .collect::<Vec<_>>()
        .join("; ");

    anyhow::bail!(
        "Delete failed for {} of {} provider nodes: {}",
        failure_count,
        providers.len(),
        failure_summary
    );
}

async fn discover_manifest_providers(
    api_client: &ApiClient,
    manifest_id: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let url = format!(
        "{}/tasks/{}/providers",
        api_client.base_url.trim_end_matches('/'),
        manifest_id
    );

    debug!("Discovering providers at: {}", url);

    // Use encrypted request to discover providers
    let response_bytes = api_client
        .send_encrypted_request(&url, &[], "providers_request")
        .await?;

    debug!("Received provider discovery response: {} bytes", response_bytes.len());

    // Parse response - binary postcard format (same as CandidatesResponse)
    let providers_response = protocol::machine::root_as_candidates_response(&response_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to parse providers response: {}", e))?;

    if !providers_response.ok() {
        debug!("Providers response indicates no providers found");
        return Ok(Vec::new());
    }

    let result: Vec<(String, String)> = providers_response
        .candidates()
        .iter()
        .map(|c| (c.peer_id.clone(), c.public_key.clone()))
        .collect();

    debug!("Parsed {} providers from response", result.len());
    Ok(result)
}

async fn send_delete_to_node(
    api_client: &ApiClient,
    node_id: &str,
    node_pubkey_b64: &str,
    manifest_id: &str,
    operation_id: &str,
    force: bool,
) -> anyhow::Result<()> {
    debug!("Sending delete request to node: {}", node_id);

    let node_pubkey_bytes = crypto::b64_decode(node_pubkey_b64)
        .map_err(|e| anyhow::anyhow!("Failed to decode node public key for {}: {}", node_id, e))?;

    let delete_request_bytes = protocol::machine::build_delete_request(
        manifest_id,
        operation_id,
        "", // origin_peer (CLI doesn't have peer ID)
        force,
    );

    let url = format!(
        "{}/delete_direct/{}",
        api_client.base_url.trim_end_matches('/'),
        node_id
    );

    debug!(
        "Sending DeleteRequest directly to node {} for manifest_id {}",
        node_id, manifest_id
    );

    // Encrypt and sign the DeleteRequest for the target worker node (end-to-end encryption)
    let response_bytes = api_client
        .send_encrypted_request_to_node(&url, &delete_request_bytes, "delete_request", &node_pubkey_bytes)
        .await?;

    let delete_response = protocol::machine::root_as_delete_response(&response_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to parse DeleteResponse: {}", e))?;

    if !delete_response.ok() {
        anyhow::bail!(
            "Direct delete failed for node {}: {}",
            node_id,
            delete_response.message().unwrap_or("unknown error")
        );
    }

    debug!("Direct delete successful for node {}", node_id);
    Ok(())
}

/// List all workloads from the scheduler.
/// Returns a JSON array of workload information.
pub async fn get_pods(api_base: Option<&str>) -> anyhow::Result<String> {
    let base = resolve_api_base(api_base);
    let url = format!("{}/runtime/workloads", base);
    
    debug!("Fetching workloads from {}", url);
    
    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to scheduler: {}", e))?;
    
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Failed to get workloads: {} - {}", status, body);
    }
    
    let body = response.text().await
        .map_err(|e| anyhow::anyhow!("Failed to read response: {}", e))?;
    
    Ok(body)
}

/// Get details of a specific workload by ID.
pub async fn get_pod(workload_id: &str, api_base: Option<&str>) -> anyhow::Result<String> {
    let base = resolve_api_base(api_base);
    let url = format!("{}/runtime/workloads/{}", base, workload_id);
    
    debug!("Fetching workload {} from {}", workload_id, url);
    
    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to scheduler: {}", e))?;
    
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("Workload '{}' not found", workload_id);
    }
    
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Failed to get workload: {} - {}", status, body);
    }
    
    let body = response.text().await
        .map_err(|e| anyhow::anyhow!("Failed to read response: {}", e))?;
    
    Ok(body)
}

/// Get logs for a specific workload.
pub async fn get_logs(workload_id: &str, tail: Option<usize>, api_base: Option<&str>) -> anyhow::Result<String> {
    let base = resolve_api_base(api_base);
    let url = if let Some(n) = tail {
        format!("{}/runtime/workloads/{}/logs?tail={}", base, workload_id, n)
    } else {
        format!("{}/runtime/workloads/{}/logs", base, workload_id)
    };
    
    debug!("Fetching logs for workload {} from {}", workload_id, url);
    
    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to scheduler: {}", e))?;
    
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("Workload '{}' not found", workload_id);
    }
    
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Failed to get logs: {} - {}", status, body);
    }
    
    let body = response.text().await
        .map_err(|e| anyhow::anyhow!("Failed to read response: {}", e))?;
    
    Ok(body)
}

/// Format workload list for human-readable output.
pub fn format_workloads_table(json_response: &str) -> String {
    let workloads: Result<Vec<serde_json::Value>, _> = serde_json::from_str(json_response);
    
    match workloads {
        Ok(list) if list.is_empty() => "No workloads found.".to_string(),
        Ok(list) => {
            let mut output = String::new();
            output.push_str(&format!(
                "{:<40} {:<15} {:<20} {:<10}\n",
                "NAME", "STATUS", "RUNTIME", "AGE"
            ));
            output.push_str(&"-".repeat(85));
            output.push('\n');
            
            for workload in list {
                let name = workload.get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let status = workload.get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let runtime = workload.get("runtime_engine")
                    .or_else(|| workload.get("runtime"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let created = workload.get("created_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                
                output.push_str(&format!(
                    "{:<40} {:<15} {:<20} {:<10}\n",
                    truncate_str(name, 38),
                    status,
                    runtime,
                    created
                ));
            }
            output
        }
        Err(_) => {
            // If we can't parse as array, just return raw JSON
            json_response.to_string()
        }
    }
}

/// Format single workload for human-readable output.
pub fn format_workload_details(json_response: &str) -> String {
    let workload: Result<serde_json::Value, _> = serde_json::from_str(json_response);
    
    match workload {
        Ok(w) => {
            let mut output = String::new();
            output.push_str("Workload Details:\n");
            output.push_str(&"-".repeat(40));
            output.push('\n');
            
            if let Some(id) = w.get("id").and_then(|v| v.as_str()) {
                output.push_str(&format!("ID:       {}\n", id));
            }
            if let Some(status) = w.get("status").and_then(|v| v.as_str()) {
                output.push_str(&format!("Status:   {}\n", status));
            }
            if let Some(runtime) = w.get("runtime_engine").or_else(|| w.get("runtime")).and_then(|v| v.as_str()) {
                output.push_str(&format!("Runtime:  {}\n", runtime));
            }
            if let Some(manifest) = w.get("manifest_id").and_then(|v| v.as_str()) {
                output.push_str(&format!("Manifest: {}\n", manifest));
            }
            if let Some(created) = w.get("created_at").and_then(|v| v.as_str()) {
                output.push_str(&format!("Created:  {}\n", created));
            }
            
            output
        }
        Err(_) => json_response.to_string(),
    }
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_selected_nodes_success() {
        let peers = vec!["peer1:dGVzdDE=".into(), "peer2:dGVzdDI=".into()];

        let parsed = parse_selected_nodes(&peers).expect("parse should succeed");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "peer1");
        assert_eq!(parsed[0].1, "dGVzdDE=");
        assert_eq!(parsed[1].0, "peer2");
    }

    #[test]
    fn test_parse_selected_nodes_invalid_format() {
        let peers = vec!["invalid".into()];
        let result = parse_selected_nodes(&peers);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_manifest_name_multi_doc_first_manifest() {
        let manifest = r#"---
apiVersion: v1
kind: ConfigMap
metadata:
    name: first
---
apiVersion: v1
kind: Pod
metadata:
    name: second
"#;

        let value = parse_manifest_to_json(manifest).expect("parse yaml");
        let name = extract_manifest_name_from_json(&value);
        assert_eq!(name.as_deref(), Some("first"));
    }

    #[test]
    fn test_extract_manifest_name_multi_doc_skips_missing_metadata() {
        let manifest = r#"---
kind: List
---
apiVersion: v1
kind: Pod
metadata:
    name: actual
"#;

        let value = parse_manifest_to_json(manifest).expect("parse yaml");
        let name = extract_manifest_name_from_json(&value);
        assert_eq!(name.as_deref(), Some("actual"));
    }
}
