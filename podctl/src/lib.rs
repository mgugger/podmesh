use anyhow::{Context, Result, anyhow};
use protocol::{
    AGENT_PROTOCOL_VERSION, AdmissionRequest, AgentAdvertisement, DeploymentGrant,
    DeploymentReceipt, EncryptedWorkloadCapsule, ExecutionSpec, Reservation, WorkloadCommand,
    WorkloadCommandResponse, WorkloadOperation,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub mod cert;

const REQUEST_TTL_SECS: u64 = 30;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn resolve_api_base(override_url: Option<&str>) -> String {
    override_url
        .map(str::to_string)
        .or_else(|| std::env::var("PODMESH_API").ok())
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(Into::into)
}

fn canonical_manifest(path: &Path) -> Result<(String, Vec<u8>)> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let documents = protocol::manifest_yaml::parse_yaml_documents_from_slice(&raw)
        .context("parse workload YAML documents")?;
    anyhow::ensure!(!documents.is_empty(), "workload manifest is empty");
    let name = documents
        .iter()
        .find(|document| {
            document.get("kind").and_then(serde_yaml::Value::as_str) == Some("Pod")
                || document
                    .get("spec")
                    .and_then(|spec| spec.get("template"))
                    .and_then(|template| template.get("spec"))
                    .is_some()
        })
        .and_then(|document| document.get("metadata"))
        .and_then(|metadata| metadata.get("name"))
        .and_then(serde_yaml::Value::as_str)
        .filter(|name| !name.is_empty() && name.len() <= 253)
        .ok_or_else(|| anyhow!("metadata.name is required"))?
        .to_string();
    let canonical = protocol::manifest_yaml::serialize_yaml_documents(&documents)?;
    Ok((name, canonical.into_bytes()))
}

async fn select_agent(scheduler_url: &str) -> Result<AgentAdvertisement> {
    let url = format!(
        "{}/api/v1/agents/select",
        scheduler_url.trim_end_matches('/')
    );
    let advertisement = http_client()?
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json::<AgentAdvertisement>()
        .await?;
    advertisement.verify(now_secs())?;
    Ok(advertisement)
}

async fn post_encrypted(agent_url: &str, endpoint: &str, payload: Vec<u8>) -> Result<Vec<u8>> {
    let url = format!("{}/api/v1/{endpoint}", agent_url.trim_end_matches('/'));
    let response = http_client()?
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(payload)
        .send()
        .await?;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        return Err(anyhow!("agent request failed with status {status}"));
    }
    Ok(body.to_vec())
}

fn encrypt_for<T: Serialize>(value: &T, recipient_kem_b64: &str) -> Result<Vec<u8>> {
    let recipient = crypto::b64_decode(recipient_kem_b64)?;
    let plaintext = postcard::to_allocvec(value)?;
    crypto::encrypt_payload_for_recipient(&recipient, &plaintext)
}

fn decrypt_from<T: for<'de> Deserialize<'de>>(body: &[u8], kem_private: &[u8]) -> Result<T> {
    let plaintext = crypto::decrypt_payload_from_recipient_blob(body, kem_private)?;
    postcard::from_bytes(&plaintext).map_err(Into::into)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalReceipt {
    receipt: DeploymentReceipt,
    agent_url: String,
    agent_kem_pubkey: String,
}

fn catalog_dir() -> Result<PathBuf> {
    let path = dirs::home_dir()
        .ok_or_else(|| anyhow!("home directory unavailable"))?
        .join(".podmesh")
        .join("workloads");
    std::fs::create_dir_all(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

fn receipt_path(workload_id: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        workload_id.len() == 64 && workload_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid workload id"
    );
    Ok(catalog_dir()?.join(format!("{workload_id}.json")))
}

fn save_receipt(receipt: &LocalReceipt) -> Result<()> {
    let path = receipt_path(&receipt.receipt.workload_id)?;
    let bytes = serde_json::to_vec(receipt)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    std::io::Write::write_all(&mut file, &bytes)?;
    Ok(())
}

fn load_receipt(workload_id: &str) -> Result<LocalReceipt> {
    let path = receipt_path(workload_id)?;
    serde_json::from_slice(&std::fs::read(path).context("workload receipt not found")?)
        .map_err(Into::into)
}

pub async fn apply_file(path: PathBuf, api_base: Option<&str>) -> Result<String> {
    let (workload_name, manifest) = canonical_manifest(&path)?;
    let (manifest, resources) = protocol::validate_and_measure_manifest(&manifest)?;
    let resources = resources.with_default_sidecar()?;
    let (owner_public, owner_private) =
        crypto::ensure_keypair_on_disk().context("load namespace signing key")?;
    let (response_kem_public, response_kem_private) =
        crypto::ensure_kem_keypair_on_disk().context("load namespace response key")?;
    let namespace_id = crypto::b64_encode(&owner_public);
    let workload_id = protocol::workload_id(&owner_public, &workload_name);
    let revision_id = protocol::revision_id(&manifest);
    let agent = select_agent(&resolve_api_base(api_base)).await?;

    let admission = AdmissionRequest {
        version: AGENT_PROTOCOL_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        namespace_id: namespace_id.clone(),
        workload_id: workload_id.clone(),
        response_kem_pubkey: crypto::b64_encode(&response_kem_public),
        cpu_milli: resources.cpu_milli,
        memory_bytes: resources.memory_bytes,
        storage_bytes: resources.storage_bytes,
        expires_at_secs: now_secs() + REQUEST_TTL_SECS,
        nonce: uuid::Uuid::new_v4().to_string(),
        owner_signature: String::new(),
    }
    .sign(&owner_private)?;
    let reservation_body = post_encrypted(
        &agent.relay_url,
        "admission",
        encrypt_for(&admission, &agent.kem_pubkey)?,
    )
    .await?;
    let reservation: Reservation = decrypt_from(&reservation_body, &response_kem_private)?;
    reservation.verify(now_secs())?;
    anyhow::ensure!(
        reservation.accepted,
        "agent rejected workload: {}",
        reservation.reason
    );
    anyhow::ensure!(
        reservation.agent_node_id == agent.node_id
            && reservation.request_id == admission.request_id
            && reservation.namespace_id == namespace_id
            && reservation.workload_id == workload_id
            && reservation.cpu_milli == admission.cpu_milli
            && reservation.memory_bytes == admission.memory_bytes
            && reservation.storage_bytes == admission.storage_bytes,
        "reservation response binding mismatch"
    );

    let execution = ExecutionSpec {
        workload_name,
        manifest,
    };
    let execution_bytes = postcard::to_allocvec(&execution)?;
    let mut dek = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut dek);
    let (ciphertext, nonce) = crypto::encrypt_payload_with_key(&dek, &execution_bytes)?;
    let agent_kem = crypto::b64_decode(&agent.kem_pubkey)?;
    let grant = DeploymentGrant {
        version: AGENT_PROTOCOL_VERSION,
        namespace_id,
        workload_id: workload_id.clone(),
        revision_id,
        target_node_id: agent.node_id.clone(),
        response_kem_pubkey: crypto::b64_encode(&response_kem_public),
        reservation_id: reservation.reservation_id,
        capsule: EncryptedWorkloadCapsule {
            ciphertext,
            nonce: nonce.to_vec(),
            wrapped_dek: crypto::encrypt_payload_for_recipient(&agent_kem, &dek)?,
        },
        issued_at_secs: now_secs(),
        expires_at_secs: now_secs() + REQUEST_TTL_SECS,
        nonce: uuid::Uuid::new_v4().to_string(),
        owner_signature: String::new(),
    }
    .sign(&owner_private)?;
    let receipt_body = post_encrypted(
        &agent.relay_url,
        "deploy",
        encrypt_for(&grant, &agent.kem_pubkey)?,
    )
    .await?;
    let receipt: DeploymentReceipt = decrypt_from(&receipt_body, &response_kem_private)?;
    receipt.verify()?;
    anyhow::ensure!(
        receipt.workload_id == workload_id && receipt.agent_node_id == agent.node_id,
        "deployment receipt binding mismatch"
    );
    save_receipt(&LocalReceipt {
        receipt,
        agent_url: agent.relay_url,
        agent_kem_pubkey: agent.kem_pubkey,
    })?;
    Ok(workload_id)
}

async fn command(
    workload_id: &str,
    operation: WorkloadOperation,
    tail: Option<usize>,
) -> Result<WorkloadCommandResponse> {
    let local = load_receipt(workload_id)?;
    let (owner_public, owner_private) = crypto::ensure_keypair_on_disk()?;
    let (response_kem_public, response_kem_private) = crypto::ensure_kem_keypair_on_disk()?;
    let command = WorkloadCommand {
        version: AGENT_PROTOCOL_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        namespace_id: crypto::b64_encode(&owner_public),
        workload_id: workload_id.to_string(),
        operation,
        log_tail: tail.map(|value| value.min(10_000) as u32),
        response_kem_pubkey: crypto::b64_encode(&response_kem_public),
        expires_at_secs: now_secs() + REQUEST_TTL_SECS,
        nonce: uuid::Uuid::new_v4().to_string(),
        owner_signature: String::new(),
    }
    .sign(&owner_private)?;
    let body = post_encrypted(
        &local.agent_url,
        "command",
        encrypt_for(&command, &local.agent_kem_pubkey)?,
    )
    .await?;
    let response: WorkloadCommandResponse = decrypt_from(&body, &response_kem_private)?;
    response.verify()?;
    anyhow::ensure!(
        response.request_id == command.request_id && response.workload_id == workload_id,
        "workload response binding mismatch"
    );
    Ok(response)
}

pub async fn delete_file(path: PathBuf, _force: bool, _api_base: Option<&str>) -> Result<String> {
    let (workload_name, _) = canonical_manifest(&path)?;
    let (owner_public, _) = crypto::ensure_keypair_on_disk()?;
    let workload_id = protocol::workload_id(&owner_public, &workload_name);
    let response = command(&workload_id, WorkloadOperation::Delete, None).await?;
    anyhow::ensure!(response.ok, "delete failed: {}", response.payload);
    std::fs::remove_file(receipt_path(&workload_id)?)?;
    Ok(workload_id)
}

pub async fn get_pod(workload_id: &str, _api_base: Option<&str>) -> Result<String> {
    let response = command(workload_id, WorkloadOperation::Status, None).await?;
    anyhow::ensure!(response.ok, "status failed: {}", response.payload);
    Ok(response.payload)
}

pub async fn get_logs(
    workload_id: &str,
    tail: Option<usize>,
    _api_base: Option<&str>,
) -> Result<String> {
    let response = command(workload_id, WorkloadOperation::Logs, tail).await?;
    anyhow::ensure!(response.ok, "logs failed: {}", response.payload);
    Ok(response.payload)
}

pub async fn get_pods(_api_base: Option<&str>) -> Result<String> {
    let mut receipts = Vec::new();
    for entry in std::fs::read_dir(catalog_dir()?)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            && let Ok(receipt) =
                serde_json::from_slice::<LocalReceipt>(&std::fs::read(entry.path())?)
        {
            receipts.push(receipt.receipt);
        }
    }
    serde_json::to_string_pretty(&receipts).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_manifest_requires_name_and_is_stable() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            temp.path(),
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: demo\n",
        )
        .unwrap();
        let first = canonical_manifest(temp.path()).unwrap();
        let second = canonical_manifest(temp.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.0, "demo");
    }

    #[test]
    fn canonical_manifest_preserves_multiple_documents() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), "kind: ConfigMap\nmetadata:\n  name: config\n---\nkind: Pod\nmetadata:\n  name: demo\nspec:\n  containers: []\n").unwrap();
        let (name, manifest) = canonical_manifest(temp.path()).unwrap();
        assert_eq!(name, "demo");
        assert_eq!(
            protocol::manifest_yaml::parse_yaml_documents_from_slice(&manifest)
                .unwrap()
                .len(),
            2
        );
    }
}
