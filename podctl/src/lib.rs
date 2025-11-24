use base64::Engine;
use crypto::{encrypt_payload_for_recipient, ensure_keypair_on_disk};
use log::debug;
use log::error;
use log::info;
use protocol::machine::parse_peer_with_pubkey;
use uuid::Uuid;

use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_yaml::{self, Value as YamlValue};
use std::env;
use std::path::PathBuf;

mod flatbuffers;
use flatbuffers::FlatbufferClient;

mod flatbuffer_envelope;

// Helper function to extract manifest name from JSON
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

fn parse_manifest_documents(contents: &str) -> anyhow::Result<JsonValue> {
    let mut docs = Vec::new();

    for document in serde_yaml::Deserializer::from_str(contents) {
        let yaml_value = YamlValue::deserialize(document)?;
        if yaml_value.is_null() {
            continue;
        }
        let json_value = serde_json::to_value(yaml_value)?;
        docs.push(json_value);
    }

    if docs.is_empty() {
        anyhow::bail!("manifest file did not contain any YAML documents");
    }

    if docs.len() == 1 {
        Ok(docs.into_iter().next().unwrap())
    } else {
        Ok(JsonValue::Array(docs))
    }
}

fn compute_manifest_id_for_owner(
    manifest_json: &JsonValue,
    owner_pubkey: &[u8],
) -> anyhow::Result<String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let manifest_name = extract_manifest_name_from_json(manifest_json).ok_or_else(|| {
        anyhow::anyhow!("Manifest must have a name field in metadata for deployment")
    })?;

    let mut hasher = DefaultHasher::new();
    owner_pubkey.hash(&mut hasher);
    manifest_name.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
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

    // Parse manifest to JSON if possible, else wrap raw
    let manifest_json = parse_manifest_documents(&contents)
        .unwrap_or_else(|_| serde_json::json!({"raw": contents}));
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

    let base = api_base
        .map(|s| s.to_string())
        .or_else(|| env::var("PODMESH_API").ok())
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());
    debug!("Creating FlatbufferClient with base URL: {}", base);
    let mut fb_client = FlatbufferClient::new(base)?;

    // Fetch machine's public key for encrypted communication
    debug!("Fetching machine's public key...");
    fb_client.fetch_machine_public_key().await?;
    debug!("Successfully fetched machine's public key");

    // 1) Get candidates for node selection
    debug!("About to call get_candidates...");
    let peers = fb_client.get_candidates(&manifest_id, replicas).await?;
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

    // Create encrypted tasks for each node sequentially with same manifest_id
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let original_manifest_str = contents.clone();
    let mut succeeded_nodes = Vec::new();
    let mut failed_nodes: Vec<(String, String)> = Vec::new();

    for (node_id, node_pubkey) in &selected_nodes {
        match send_apply_to_node(
            &fb_client,
            node_id,
            node_pubkey,
            &original_manifest_str,
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
                failed_nodes.push((node_id.clone(), err_msg));
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
    fb_client: &FlatbufferClient,
    node_id: &str,
    node_pubkey_b64: &str,
    manifest_payload: &str,
    manifest_id: &str,
    timestamp_ms: u64,
) -> anyhow::Result<()> {
    debug!("Creating encrypted task for node: {}", node_id);

    let node_pubkey_bytes = base64::engine::general_purpose::STANDARD
        .decode(node_pubkey_b64)
        .map_err(|e| anyhow::anyhow!("Failed to decode node public key for {}: {}", node_id, e))?;

    let encrypted_blob =
        encrypt_payload_for_recipient(&node_pubkey_bytes, manifest_payload.as_bytes())?;

    let encrypted_manifest_bytes = protocol::machine::build_envelope_canonical_with_peer(
        &encrypted_blob,
        "manifest",
        "",
        timestamp_ms,
        "ml-kem-512",
        "",
        None,
    );

    debug!(
        "Sending ApplyRequest directly to node {} with manifest_id {}",
        node_id, manifest_id
    );

    let operation_id = Uuid::new_v4().to_string();
    let manifest_json_b64 =
        base64::engine::general_purpose::STANDARD.encode(&encrypted_manifest_bytes);

    let apply_request_bytes = protocol::machine::build_apply_request(
        1,
        &operation_id,
        &manifest_json_b64,
        "",
        manifest_id,
    );

    let url = format!(
        "{}/apply_direct/{}",
        fb_client.base_url.trim_end_matches('/'),
        node_id
    );

    let response_bytes = fb_client
        .send_encrypted_request(&url, &apply_request_bytes, "apply_request")
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
    let manifest_json = parse_manifest_documents(&contents)
        .unwrap_or_else(|_| serde_json::json!({"raw": contents}));
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
    debug!("Creating FlatbufferClient with base URL: {}", base);
    let mut fb_client = FlatbufferClient::new(base)?;

    // Fetch machine's public key for encrypted communication
    debug!("Fetching machine's public key...");
    fb_client.fetch_machine_public_key().await?;
    debug!("Successfully fetched machine's public key");

    // Build delete request
    let operation_id = Uuid::new_v4().to_string();
    let delete_request_bytes = protocol::machine::build_delete_request(
        &manifest_id,
        &operation_id,
        "", // origin_peer (CLI doesn't have peer ID)
        force,
    );

    // Send delete request via REST API
    let url = format!(
        "{}/tasks/{}",
        fb_client.base_url.trim_end_matches('/'),
        manifest_id
    );

    debug!("Sending delete request to: {}", url);

    let response_bytes = fb_client
        .send_delete_request(&url, &delete_request_bytes)
        .await?;

    // Parse response as DeleteResponse
    let delete_response = protocol::machine::root_as_delete_response(&response_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to parse DeleteResponse: {}", e))?;

    if !delete_response.ok() {
        anyhow::bail!(
            "Delete failed: {}",
            delete_response.message().unwrap_or("unknown error")
        );
    }

    info!(
        "Delete completed for manifest_id {}: {}",
        manifest_id,
        delete_response.message().unwrap_or("success")
    );

    Ok(manifest_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_selected_nodes_success() {
        let peers = vec!["peer1:dGVzdDE=".to_string(), "peer2:dGVzdDI=".to_string()];

        let parsed = parse_selected_nodes(&peers).expect("parse should succeed");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "peer1");
        assert_eq!(parsed[0].1, "dGVzdDE=");
        assert_eq!(parsed[1].0, "peer2");
    }

    #[test]
    fn test_parse_selected_nodes_invalid_format() {
        let peers = vec!["invalid".to_string()];
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

        let value = parse_manifest_documents(manifest).expect("parse yaml");
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

        let value = parse_manifest_documents(manifest).expect("parse yaml");
        let name = extract_manifest_name_from_json(&value);
        assert_eq!(name.as_deref(), Some("actual"));
    }
}
