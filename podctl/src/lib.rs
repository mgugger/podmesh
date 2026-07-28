use anyhow::{Context, Result, anyhow};
use protocol::{
    AGENT_PROTOCOL_VERSION, AdmissionRequest, CapacityOffer, DeploymentGrant, DeploymentReceipt,
    EncryptedWorkloadCapsule, ExecutionSpec, Reservation, WorkloadCommand, WorkloadCommandResponse,
    WorkloadOperation,
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
/// Comma-separated proxy REST base URLs that `podctl` asks for the endpoint
/// record, relay token and relay certificate of each ingress proxy.
const PROXY_URL_ENV_VAR: &str = "PODMESH_PROXY_URL";
/// Bounds how many proxies a single deployment may be bootstrapped from, so a
/// runaway environment variable cannot fan out unboundedly.
const MAX_BOOTSTRAP_PROXIES: usize = 8;
/// Lifetime of the grants `podctl apply` mints. Short enough that a retired
/// proxy loses authority on its own, long enough to survive normal operation.
const PROXY_GRANT_TTL_DAYS: u64 = 30;

/// Response body of the proxy's `GET /api/v1/workload_relay_bootstrap`.
#[derive(Deserialize)]
struct WorkloadRelayBootstrap {
    endpoint_record_b64: String,
    auth_token: String,
    ca_certificate_b64: String,
}

/// Collects proxy endpoint records and workload relay credentials directly from
/// the proxies, so an operator never has to copy a relay secret by hand.
///
/// Every proxy in a deployment must share one relay token, because the injected
/// sidecar is handed exactly one token to present. Disagreeing proxies are a
/// misconfiguration and are rejected rather than silently half-working.
async fn bootstrap_from_proxies(proxy_urls: &str) -> Result<(Vec<String>, String, Vec<Vec<u8>>)> {
    let urls: Vec<&str> = proxy_urls
        .split(',')
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .collect();
    anyhow::ensure!(
        !urls.is_empty() && urls.len() <= MAX_BOOTSTRAP_PROXIES,
        "{PROXY_URL_ENV_VAR} must list between 1 and {MAX_BOOTSTRAP_PROXIES} proxy URLs"
    );

    let client = http_client()?;
    let mut endpoints = Vec::with_capacity(urls.len());
    let mut certificates = Vec::with_capacity(urls.len());
    let mut auth_token: Option<String> = None;
    for url in urls {
        let bootstrap: WorkloadRelayBootstrap = client
            .get(format!(
                "{}/api/v1/workload_relay_bootstrap",
                url.trim_end_matches('/')
            ))
            .send()
            .await
            .with_context(|| format!("reach proxy {url}"))?
            .error_for_status()
            .with_context(|| {
                format!("proxy {url} refused to publish relay credentials; start it with --publish-relay-bootstrap")
            })?
            .json()
            .await
            .with_context(|| format!("decode relay bootstrap from proxy {url}"))?;

        match &auth_token {
            None => auth_token = Some(bootstrap.auth_token),
            Some(existing) => anyhow::ensure!(
                existing == &bootstrap.auth_token,
                "proxies in {PROXY_URL_ENV_VAR} disagree on the workload relay token"
            ),
        }
        endpoints.push(bootstrap.endpoint_record_b64);
        certificates.push(crypto::b64_decode(&bootstrap.ca_certificate_b64)?);
    }
    let auth_token = auth_token.context("no proxy returned a workload relay token")?;
    Ok((endpoints, auth_token, certificates))
}

/// Mints an owner-signed grant for every proxy this deployment will use.
///
/// `podctl` holds the namespace owner's Ed25519 key, so it is the only party
/// that can authorize a proxy to front this tenant. Each proxy gets a Biscuit
/// naming its own endpoint; a sidecar later verifies that Biscuit using only
/// the owner public key it received in its encrypted metadata.
async fn grant_proxies(proxy_urls: &str, owner_public: &[u8], owner_private: &[u8]) -> Result<()> {
    let urls: Vec<&str> = proxy_urls
        .split(',')
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .collect();
    anyhow::ensure!(
        !urls.is_empty() && urls.len() <= MAX_BOOTSTRAP_PROXIES,
        "{PROXY_URL_ENV_VAR} must list between 1 and {MAX_BOOTSTRAP_PROXIES} proxy URLs"
    );
    for url in urls {
        crate::cert::grant_proxy_async(url, owner_public, owner_private, PROXY_GRANT_TTL_DAYS)
            .await
            .with_context(|| format!("grant proxy {url} authority for this namespace"))?;
    }
    Ok(())
}

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

/// Parses the workload manifest, pinning every document to a single pod and
/// reporting how many replicas the owner asked for. Replica spreading is a
/// client-side decision: `podctl` selects one agent per replica and deploys the
/// same single-pod manifest to each of them.
fn canonical_manifest(path: &Path) -> Result<(String, u32, Vec<u8>)> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut documents = protocol::manifest_yaml::parse_yaml_documents_from_slice(&raw)
        .context("parse workload YAML documents")?;
    anyhow::ensure!(!documents.is_empty(), "workload manifest is empty");
    let replicas = protocol::manifest_yaml::normalize_replicas(&mut documents);
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
    Ok((name, replicas, canonical.into_bytes()))
}

/// Asks a scheduler for one agent that can host the next replica. `exclude`
/// carries the agents this deployment already occupies so the mesh answers with
/// a different one and replicas never share a host.
async fn select_agent(scheduler_url: &str, exclude: &[String]) -> Result<CapacityOffer> {
    let mut url = format!(
        "{}/api/v1/agents/select",
        scheduler_url.trim_end_matches('/')
    );
    if !exclude.is_empty() {
        url.push_str("?exclude=");
        url.push_str(&exclude.join(","));
    }
    let offer = http_client()?
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json::<CapacityOffer>()
        .await?;
    offer.verify(now_secs())?;
    Ok(offer)
}

/// Lowercase hex form of the agent's Iroh EndpointId, used to address the agent
/// through a scheduler. `podctl` has no Iroh endpoint of its own, so it never
/// dials an agent directly.
fn agent_endpoint_id(offer: &CapacityOffer) -> Result<String> {
    anyhow::ensure!(
        offer.agent_endpoint.endpoint_id.len() == protocol::IROH_ENDPOINT_ID_BYTES,
        "capacity offer carries a malformed agent EndpointId"
    );
    Ok(hex::encode(&offer.agent_endpoint.endpoint_id))
}

/// Relays an owner-encrypted payload to an agent through a scheduler. The
/// scheduler cannot read or forge the payload; it only carries opaque bytes
/// over its authenticated Iroh connection to the agent.
async fn post_encrypted(
    api_base: &str,
    agent_endpoint_id: &str,
    operation: &str,
    payload: Vec<u8>,
) -> Result<Vec<u8>> {
    let url = format!(
        "{}/api/v1/agents/{agent_endpoint_id}/{operation}",
        api_base.trim_end_matches('/')
    );
    let response = http_client()?
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(payload)
        .send()
        .await?;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        return Err(anyhow!(
            "scheduler could not complete {operation} on agent {agent_endpoint_id}: status {status}"
        ));
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

/// One replica of a deployment, pinned to the agent `podctl` selected for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplicaPlacement {
    replica_index: u32,
    receipt: DeploymentReceipt,
    /// Scheduler HTTP endpoint that relayed this replica. Lifecycle commands
    /// go back through a scheduler, never straight to the agent.
    api_base: String,
    agent_endpoint_id: String,
    agent_kem_pubkey: String,
}

/// Everything `podctl` needs to reach every replica of one deployment again.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeploymentCatalog {
    deployment_id: String,
    workload_name: String,
    replicas: Vec<ReplicaPlacement>,
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

fn catalog_path(deployment_id: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        deployment_id.len() == 64 && deployment_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid deployment id"
    );
    Ok(catalog_dir()?.join(format!("{deployment_id}.json")))
}

fn save_catalog(catalog: &DeploymentCatalog) -> Result<()> {
    let path = catalog_path(&catalog.deployment_id)?;
    let bytes = serde_json::to_vec(catalog)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    std::io::Write::write_all(&mut file, &bytes)?;
    Ok(())
}

/// Loads a deployment catalog by deployment ID or by workload name.
///
/// Names are resolved against the local catalog directory only. `podctl` keeps
/// no cluster-side index, so a name that was applied from another machine is
/// unknown here and must be addressed by its deployment ID.
fn load_catalog(identifier: &str) -> Result<DeploymentCatalog> {
    if let Ok(path) = catalog_path(identifier) {
        return serde_json::from_slice(
            &std::fs::read(path).context("deployment catalog not found")?,
        )
        .map_err(Into::into);
    }
    let mut matched: Option<DeploymentCatalog> = None;
    for entry in std::fs::read_dir(catalog_dir()?)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(catalog) =
            serde_json::from_slice::<DeploymentCatalog>(&std::fs::read(entry.path())?)
        else {
            continue;
        };
        if catalog.workload_name != identifier {
            continue;
        }
        anyhow::ensure!(
            matched.is_none(),
            "workload name {identifier} is ambiguous, use the deployment id"
        );
        matched = Some(catalog);
    }
    matched.ok_or_else(|| anyhow!("no deployment named {identifier}"))
}

pub async fn apply_file(path: PathBuf, api_base: Option<&str>) -> Result<String> {
    apply_file_internal(path, api_base, None, None).await
}

/// Applies a manifest against an explicit proxy URL list instead of reading
/// `PODMESH_PROXY_URL` from the environment.
pub async fn apply_file_with_proxy_urls(
    path: PathBuf,
    api_base: Option<&str>,
    proxy_urls: String,
) -> Result<String> {
    apply_file_internal(path, api_base, None, Some(proxy_urls)).await
}

pub async fn apply_file_with_proxy_endpoints(
    path: PathBuf,
    api_base: Option<&str>,
    proxy_endpoints: Vec<String>,
    workload_relay_auth_token: String,
    workload_relay_ca_certificates: Vec<Vec<u8>>,
) -> Result<String> {
    apply_file_internal(
        path,
        api_base,
        Some((
            proxy_endpoints,
            workload_relay_auth_token,
            workload_relay_ca_certificates,
        )),
        None,
    )
    .await
}

async fn apply_file_internal(
    path: PathBuf,
    api_base: Option<&str>,
    explicit_proxy_config: Option<(Vec<String>, String, Vec<Vec<u8>>)>,
    proxy_urls: Option<String>,
) -> Result<String> {
    let proxy_urls = proxy_urls.or_else(|| {
        // An explicit proxy configuration fully describes the proxies to use,
        // so the ambient environment must not add proxies to grant on top.
        if explicit_proxy_config.is_some() {
            None
        } else {
            std::env::var(PROXY_URL_ENV_VAR).ok()
        }
    });
    let (workload_name, replica_count, manifest) = canonical_manifest(&path)?;
    let annotations = protocol::PodmeshAnnotations::from_manifest_yaml(
        std::str::from_utf8(&manifest).context("manifest is not UTF-8")?,
    )?;
    let (encoded_proxy_endpoints, workload_relay_auth_token, workload_relay_ca_certificates) =
        if let Some(explicit) = explicit_proxy_config {
            explicit
        } else if let Some(proxy_urls) = proxy_urls.as_deref() {
            bootstrap_from_proxies(proxy_urls).await?
        } else {
            let endpoints = if annotations.proxy_endpoints.is_empty() {
                std::env::var("PODMESH_PROXY_ENDPOINTS")
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|endpoint| !endpoint.is_empty())
                    .map(str::to_string)
                    .collect()
            } else {
                annotations.proxy_endpoints
            };
            let token = std::env::var("PODMESH_WORKLOAD_RELAY_AUTH_TOKEN").context(
                "set PODMESH_PROXY_URL to bootstrap from a proxy, or supply \
                 PODMESH_WORKLOAD_RELAY_AUTH_TOKEN explicitly",
            )?;
            let certificates = std::env::var("PODMESH_WORKLOAD_RELAY_CA_CERTS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(crypto::b64_decode)
                .collect::<Result<Vec<_>>>()?;
            (endpoints, token, certificates)
        };
    let now = now_secs();
    let proxy_endpoints = encoded_proxy_endpoints
        .iter()
        .map(|encoded| {
            let bytes = crypto::b64_decode(encoded).context("decode proxy EndpointRecord")?;
            protocol::EndpointRecord::from_bytes(&bytes, now)
        })
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(
        !proxy_endpoints.is_empty(),
        "initial proxy EndpointRecords are required in podmesh.io/proxy-endpoints or PODMESH_PROXY_ENDPOINTS"
    );
    let (manifest, resources) = protocol::validate_and_measure_manifest(&manifest)?;
    let resources = resources.with_default_sidecar()?;
    let (owner_public, owner_private) =
        crypto::ensure_keypair_on_disk().context("load namespace signing key")?;
    let (response_kem_public, response_kem_private) =
        crypto::ensure_kem_keypair_on_disk().context("load namespace response key")?;
    let namespace_id = crypto::b64_encode(&owner_public);
    // The owner authorizes each proxy explicitly. Without a grant a proxy will
    // not answer this tenant's sidecars, so it is provisioned as part of the
    // deployment rather than as a separate manual step.
    if let Some(proxy_urls) = proxy_urls.as_deref() {
        grant_proxies(proxy_urls, &owner_public, &owner_private).await?;
    }
    let deployment_id = protocol::deployment_id(&owner_public, &workload_name);
    let revision_id = protocol::revision_id(&manifest);
    let api_base = resolve_api_base(api_base);

    // `podctl` places the replicas itself: for every replica it asks a
    // scheduler for one agent, excluding the agents this deployment already
    // occupies, then admits and deploys directly against that agent. The
    // scheduler never decides how many replicas exist or where they go.
    let mut placements = Vec::with_capacity(replica_count as usize);
    let mut occupied_agents = Vec::with_capacity(replica_count as usize);
    for replica_index in 0..replica_count {
        let agent = select_agent(&api_base, &occupied_agents)
            .await
            .with_context(|| {
                format!(
                    "no agent available for replica {} of {replica_count}; \
                     each replica needs its own agent",
                    replica_index + 1
                )
            })?;
        let agent_endpoint_id = agent_endpoint_id(&agent)?;
        anyhow::ensure!(
            !occupied_agents.contains(&agent_endpoint_id),
            "scheduler offered agent {agent_endpoint_id} twice; replicas must not share a host"
        );
        let placement = deploy_replica(
            ReplicaRequest {
                api_base: &api_base,
                namespace_id: &namespace_id,
                workload_name: &workload_name,
                manifest: &manifest,
                revision_id: &revision_id,
                replica_index,
                replica_count,
                proxy_endpoints: &proxy_endpoints,
                workload_relay_auth_token: &workload_relay_auth_token,
                workload_relay_ca_certificates: &workload_relay_ca_certificates,
                resources: &resources,
            },
            &agent,
            &agent_endpoint_id,
            (&owner_public, &owner_private),
            (&response_kem_public, &response_kem_private),
        )
        .await
        .with_context(|| format!("deploying replica {replica_index} to {agent_endpoint_id}"))?;
        occupied_agents.push(agent_endpoint_id);
        placements.push(placement);
    }

    save_catalog(&DeploymentCatalog {
        deployment_id: deployment_id.clone(),
        workload_name,
        replicas: placements,
    })?;
    Ok(deployment_id)
}

/// Everything that is identical across the replicas of one deployment.
struct ReplicaRequest<'a> {
    api_base: &'a str,
    namespace_id: &'a str,
    workload_name: &'a str,
    manifest: &'a [u8],
    revision_id: &'a str,
    replica_index: u32,
    replica_count: u32,
    proxy_endpoints: &'a [protocol::EndpointRecord],
    workload_relay_auth_token: &'a str,
    workload_relay_ca_certificates: &'a [Vec<u8>],
    resources: &'a protocol::ManifestResources,
}

/// Admits and deploys a single replica onto one already-selected agent.
async fn deploy_replica(
    request: ReplicaRequest<'_>,
    agent: &CapacityOffer,
    agent_endpoint_id: &str,
    owner_keys: (&[u8], &[u8]),
    response_kem_keys: (&[u8], &[u8]),
) -> Result<ReplicaPlacement> {
    let (owner_public, owner_private) = owner_keys;
    let (response_kem_public, response_kem_private) = response_kem_keys;
    let workload_id =
        protocol::workload_id(owner_public, request.workload_name, request.replica_index);

    let admission = AdmissionRequest {
        version: AGENT_PROTOCOL_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        namespace_id: request.namespace_id.to_string(),
        workload_id: workload_id.clone(),
        response_kem_pubkey: crypto::b64_encode(response_kem_public),
        cpu_milli: request.resources.cpu_milli,
        memory_bytes: request.resources.memory_bytes,
        storage_bytes: request.resources.storage_bytes,
        expires_at_secs: now_secs() + REQUEST_TTL_SECS,
        nonce: uuid::Uuid::new_v4().to_string(),
        owner_signature: String::new(),
    }
    .sign(owner_private)?;
    let reservation_body = post_encrypted(
        request.api_base,
        agent_endpoint_id,
        "admission",
        encrypt_for(&admission, &agent.kem_pubkey)?,
    )
    .await?;
    let reservation: Reservation = decrypt_from(&reservation_body, response_kem_private)?;
    reservation.verify(now_secs())?;
    anyhow::ensure!(
        reservation.accepted,
        "agent rejected workload: {}",
        reservation.reason
    );
    anyhow::ensure!(
        reservation.agent_node_id == agent.signing_pubkey
            && reservation.request_id == admission.request_id
            && reservation.namespace_id == request.namespace_id
            && reservation.workload_id == workload_id
            && reservation.cpu_milli == admission.cpu_milli
            && reservation.memory_bytes == admission.memory_bytes
            && reservation.storage_bytes == admission.storage_bytes,
        "reservation response binding mismatch"
    );

    let execution = ExecutionSpec {
        workload_name: request.workload_name.to_string(),
        replica_index: request.replica_index,
        replica_count: request.replica_count,
        manifest: request.manifest.to_vec(),
        proxy_endpoints: request.proxy_endpoints.to_vec(),
        workload_relay_auth_token: request.workload_relay_auth_token.to_string(),
        workload_relay_ca_certificates: request.workload_relay_ca_certificates.to_vec(),
    };
    let execution_bytes = postcard::to_allocvec(&execution)?;
    let mut dek = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut dek);
    let (ciphertext, nonce) = crypto::encrypt_payload_with_key(&dek, &execution_bytes)?;
    let agent_kem = crypto::b64_decode(&agent.kem_pubkey)?;
    let grant = DeploymentGrant {
        version: AGENT_PROTOCOL_VERSION,
        namespace_id: request.namespace_id.to_string(),
        workload_id: workload_id.clone(),
        revision_id: request.revision_id.to_string(),
        target_node_id: agent.signing_pubkey.clone(),
        response_kem_pubkey: crypto::b64_encode(response_kem_public),
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
    .sign(owner_private)?;
    let receipt_body = post_encrypted(
        request.api_base,
        agent_endpoint_id,
        "deploy",
        encrypt_for(&grant, &agent.kem_pubkey)?,
    )
    .await?;
    let receipt: DeploymentReceipt = decrypt_from(&receipt_body, response_kem_private)?;
    receipt.verify()?;
    anyhow::ensure!(
        receipt.workload_id == workload_id && receipt.agent_node_id == agent.signing_pubkey,
        "deployment receipt binding mismatch"
    );
    Ok(ReplicaPlacement {
        replica_index: request.replica_index,
        receipt,
        api_base: request.api_base.to_string(),
        agent_endpoint_id: agent_endpoint_id.to_string(),
        agent_kem_pubkey: agent.kem_pubkey.clone(),
    })
}

/// Sends one owner-signed lifecycle command to the agent holding a replica.
async fn command(
    placement: &ReplicaPlacement,
    operation: WorkloadOperation,
    tail: Option<usize>,
    api_base: Option<&str>,
) -> Result<WorkloadCommandResponse> {
    let workload_id = &placement.receipt.workload_id;
    let (owner_public, owner_private) = crypto::ensure_keypair_on_disk()?;
    let (response_kem_public, response_kem_private) = crypto::ensure_kem_keypair_on_disk()?;
    let command = WorkloadCommand {
        version: AGENT_PROTOCOL_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        namespace_id: crypto::b64_encode(&owner_public),
        workload_id: workload_id.clone(),
        operation,
        log_tail: tail.map(|value| value.min(10_000) as u32),
        response_kem_pubkey: crypto::b64_encode(&response_kem_public),
        expires_at_secs: now_secs() + REQUEST_TTL_SECS,
        nonce: uuid::Uuid::new_v4().to_string(),
        owner_signature: String::new(),
    }
    .sign(&owner_private)?;
    let body = post_encrypted(
        api_base.unwrap_or(&placement.api_base),
        &placement.agent_endpoint_id,
        "command",
        encrypt_for(&command, &placement.agent_kem_pubkey)?,
    )
    .await?;
    let response: WorkloadCommandResponse = decrypt_from(&body, &response_kem_private)?;
    response.verify()?;
    anyhow::ensure!(
        response.request_id == command.request_id && &response.workload_id == workload_id,
        "workload response binding mismatch"
    );
    Ok(response)
}

/// Runs one lifecycle command against every replica of a deployment and reports
/// the per-replica results in replica order.
async fn command_all(
    deployment_id: &str,
    operation: WorkloadOperation,
    tail: Option<usize>,
    api_base: Option<&str>,
) -> Result<(DeploymentCatalog, Vec<WorkloadCommandResponse>)> {
    let catalog = load_catalog(deployment_id)?;
    let mut responses = Vec::with_capacity(catalog.replicas.len());
    for placement in &catalog.replicas {
        responses.push(
            command(placement, operation, tail, api_base)
                .await
                .with_context(|| {
                    format!(
                        "replica {} on agent {}",
                        placement.replica_index, placement.agent_endpoint_id
                    )
                })?,
        );
    }
    Ok((catalog, responses))
}

/// Renders per-replica payloads as a JSON array so a multi-replica deployment
/// reports every replica instead of an arbitrary one.
fn render_replica_payloads(
    catalog: &DeploymentCatalog,
    responses: &[WorkloadCommandResponse],
    operation: &str,
) -> Result<String> {
    let mut rendered = Vec::with_capacity(responses.len());
    for (placement, response) in catalog.replicas.iter().zip(responses) {
        anyhow::ensure!(
            response.ok,
            "{operation} failed for replica {} on agent {}: {}",
            placement.replica_index,
            placement.agent_endpoint_id,
            response.payload
        );
        rendered.push(serde_json::json!({
            "replica_index": placement.replica_index,
            "workload_id": response.workload_id,
            "agent_endpoint_id": placement.agent_endpoint_id,
            "payload": response.payload,
        }));
    }
    serde_json::to_string_pretty(&rendered).map_err(Into::into)
}

/// Deletes every replica of a deployment. The local catalog is only dropped
/// once all agents confirmed, so a partial failure leaves the remaining
/// replicas addressable.
pub async fn delete_file(path: PathBuf, _force: bool, api_base: Option<&str>) -> Result<String> {
    let (workload_name, _, _) = canonical_manifest(&path)?;
    let (owner_public, _) = crypto::ensure_keypair_on_disk()?;
    let deployment_id = protocol::deployment_id(&owner_public, &workload_name);
    let (catalog, responses) =
        command_all(&deployment_id, WorkloadOperation::Delete, None, api_base).await?;
    for (placement, response) in catalog.replicas.iter().zip(&responses) {
        anyhow::ensure!(
            response.ok,
            "delete failed for replica {} on agent {}: {}",
            placement.replica_index,
            placement.agent_endpoint_id,
            response.payload
        );
    }
    std::fs::remove_file(catalog_path(&deployment_id)?)?;
    Ok(deployment_id)
}

pub async fn get_pod(deployment_id: &str, api_base: Option<&str>) -> Result<String> {
    let (catalog, responses) =
        command_all(deployment_id, WorkloadOperation::Status, None, api_base).await?;
    render_replica_payloads(&catalog, &responses, "status")
}

pub async fn get_logs(
    deployment_id: &str,
    tail: Option<usize>,
    api_base: Option<&str>,
) -> Result<String> {
    let (catalog, responses) =
        command_all(deployment_id, WorkloadOperation::Logs, tail, api_base).await?;
    render_replica_payloads(&catalog, &responses, "logs")
}

pub async fn get_pods(_api_base: Option<&str>) -> Result<String> {
    let mut deployments = Vec::new();
    for entry in std::fs::read_dir(catalog_dir()?)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            && let Ok(catalog) =
                serde_json::from_slice::<DeploymentCatalog>(&std::fs::read(entry.path())?)
        {
            deployments.push(serde_json::json!({
                "deployment_id": catalog.deployment_id,
                "workload_name": catalog.workload_name,
                "replicas": catalog
                    .replicas
                    .iter()
                    .map(|placement| serde_json::json!({
                        "replica_index": placement.replica_index,
                        "agent_endpoint_id": placement.agent_endpoint_id,
                        "receipt": placement.receipt,
                    }))
                    .collect::<Vec<_>>(),
            }));
        }
    }
    serde_json::to_string_pretty(&deployments).map_err(Into::into)
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
        let (name, replicas, manifest) = canonical_manifest(temp.path()).unwrap();
        assert_eq!(name, "demo");
        assert_eq!(replicas, 1);
        assert_eq!(
            protocol::manifest_yaml::parse_yaml_documents_from_slice(&manifest)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn canonical_manifest_lifts_replicas_out_and_pins_the_pod_count_to_one() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            temp.path(),
            "kind: Deployment\nmetadata:\n  name: demo\nspec:\n  replicas: 3\n  template:\n    spec:\n      containers: []\n",
        )
        .unwrap();
        let (name, replicas, manifest) = canonical_manifest(temp.path()).unwrap();
        assert_eq!(name, "demo");
        assert_eq!(replicas, 3);
        let documents =
            protocol::manifest_yaml::parse_yaml_documents_from_slice(&manifest).unwrap();
        assert_eq!(
            documents[0]
                .get("spec")
                .and_then(|spec| spec.get("replicas")),
            Some(&serde_yaml::Value::Number(1.into())),
            "each agent must run exactly one pod for its replica"
        );
    }
}
