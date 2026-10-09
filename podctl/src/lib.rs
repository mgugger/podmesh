use anyhow::{Context, Result, anyhow};
use protocol::{
    AGENT_PROTOCOL_VERSION, AdmissionRequest, CapacityOffer, DeploymentGrant, DeploymentReceipt,
    EncryptedWorkloadCapsule, ExecutionSpec, Reservation, UpdateOutcome, UpdateRequest,
    UpdateResponse, WorkloadCommand, WorkloadCommandResponse, WorkloadOperation,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

mod bootstrap;
pub mod catalog;
pub mod cert;
pub mod trust;

use catalog::{DeploymentCatalog, ReplicaPlacement, ReplicaUpdateState};
pub use trust::AgentTrust;

/// Overrides the key and catalog directory. Every component in a test, and any
/// operator running more than one identity on a host, needs this to be explicit
/// rather than derived from the process environment alone.
pub const KEY_DIR_ENV_VAR: &str = "PODMESH_KEY_DIR";

/// Options that apply to every command.
#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    pub api_base: Option<String>,
    /// Accept whichever agent the mesh offers instead of consulting the
    /// owner's trusted agent list.
    pub trust_any_agent: bool,
}

impl ClientOptions {
    pub fn with_api_base(api_base: Option<&str>) -> Self {
        Self {
            api_base: api_base.map(str::to_string),
            trust_any_agent: false,
        }
    }

    fn api_base(&self) -> Option<&str> {
        self.api_base.as_deref()
    }
}

/// Directory holding this owner's keys and deployment catalog.
pub fn key_dir() -> Result<PathBuf> {
    match std::env::var(KEY_DIR_ENV_VAR) {
        Ok(value) if !value.trim().is_empty() => Ok(PathBuf::from(value)),
        _ => crypto::default_key_dir(),
    }
}

fn owner_keys(key_dir: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    crypto::load_or_create_signing_keypair(key_dir).context("load namespace signing key")
}

fn response_keys(key_dir: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    crypto::load_or_create_kem_keypair(key_dir).context("load namespace response key")
}

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
/// Lifetime of the credential a pod uses to prove its tenancy to a proxy.
///
/// Matched to the proxy grant, because both are re-minted by the same deploy
/// and a pod outliving its credential would silently lose ingress.
const WORKLOAD_CREDENTIAL_TTL_SECS: u64 = PROXY_GRANT_TTL_DAYS * 24 * 60 * 60;
const SERVICE_MESH_RENEWAL_WINDOW_SECS: u64 = 24 * 60 * 60;
const MAX_ADMISSION_CAPACITY_RETRIES: usize = 3;

#[derive(Debug)]
struct AdmissionCapacityRefusal {
    reason: String,
}

impl std::fmt::Display for AdmissionCapacityRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "agent rejected workload capacity: {}",
            self.reason
        )
    }
}

impl std::error::Error for AdmissionCapacityRefusal {}

fn is_capacity_refusal(reason: &str) -> bool {
    protocol::agent::AdmissionRefusal::from_reason(reason)
        .is_some_and(protocol::agent::AdmissionRefusal::is_capacity)
}

fn service_mesh_renewal_due(placement: &ReplicaPlacement, now: u64) -> bool {
    placement.service_mesh_expires_at_secs <= now.saturating_add(SERVICE_MESH_RENEWAL_WINDOW_SECS)
}

/// Response body of the proxy's `GET /api/v1/workload_relay_bootstrap`.
///
/// The endpoint record it also carries is ignored here: it is read from the
/// public `GET /api/v1/endpoint_record` for every proxy, including those that
/// do not publish a token.
#[derive(Deserialize)]
struct WorkloadRelayBootstrap {
    auth_token: String,
    ca_certificate_b64: String,
}

/// Collects proxy endpoint records and workload relay credentials directly from
/// the proxies, so an operator never has to copy a relay secret by hand.
///
/// The two halves come from different endpoints on purpose. An `EndpointRecord`
/// is public — it is signed and self-expiring and is what lets a sidecar dial a
/// proxy — so every listed proxy must serve one. The relay bootstrap endpoint
/// discloses a live token in cleartext, so only the proxies that opted into
/// publishing it are asked, and at least one must answer.
///
/// The relay token is derived per tenant, so `namespace_id` is sent with the
/// request and the proxy answers with this tenant's token only — never the mesh
/// secret it derives from. Every proxy derives the same token for a tenant, so
/// publishers that disagree indicate different mesh secrets and are rejected
/// rather than silently half-working: the injected sidecar carries one token.
async fn bootstrap_from_proxies(
    proxy_urls: &str,
    namespace_id: &str,
    key_dir: &Path,
) -> Result<(Vec<String>, String, Vec<Vec<u8>>)> {
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
    let mut certificates = Vec::new();
    let mut auth_token: Option<String> = None;
    let proxy_trust = trust::load_registry(key_dir)?;

    for url in urls {
        let observed = bootstrap::fetch_proxy_identity(&client, url, now_secs()).await?;
        proxy_trust.authorize_proxy(&observed.identity)?;
        let base = observed.identity.origin.as_str();
        endpoints.push(observed.endpoint_record_b64);

        let (status, body) = bootstrap::send_bounded(
            client
                .get(format!("{base}/api/v1/workload_relay_bootstrap"))
                .query(&[("owner", namespace_id)]),
        )
        .await
        .with_context(|| format!("reach proxy {url}"))?;
        if status == reqwest::StatusCode::NOT_FOUND {
            // This proxy adopted its token from a peer rather than publishing
            // it, which is the intended posture for all but one proxy.
            continue;
        }
        anyhow::ensure!(
            status.is_success(),
            "proxy {url} refused to publish relay credentials: status {status}"
        );
        let bootstrap: WorkloadRelayBootstrap = serde_json::from_slice(&body)
            .with_context(|| format!("decode relay bootstrap from proxy {url}"))?;

        match &auth_token {
            None => auth_token = Some(bootstrap.auth_token),
            Some(existing) => anyhow::ensure!(
                existing == &bootstrap.auth_token,
                "proxies in {PROXY_URL_ENV_VAR} disagree on the workload relay token"
            ),
        }
        certificates.push(crypto::b64_decode(&bootstrap.ca_certificate_b64)?);
    }

    let auth_token = auth_token.with_context(|| {
        format!(
            "no proxy in {PROXY_URL_ENV_VAR} published a workload relay token; \
             start exactly one of them with --publish-relay-bootstrap"
        )
    })?;
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
    let mut failures = Vec::new();
    for url in urls {
        if let Err(error) =
            crate::cert::grant_proxy_async(url, owner_public, owner_private, PROXY_GRANT_TTL_DAYS)
                .await
        {
            failures.push(format!("{url}: {error:#}"));
        }
    }
    if !failures.is_empty() {
        return Err(anyhow!(
            "proxy grant renewal failed while existing unexpired grants remain usable: {}",
            failures.join("; ")
        ));
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
///
/// The returned offer is checked against the owner's trusted agent list before
/// it is used. `offer.verify` only proves the offer is internally consistent —
/// the signing key is inside the message — so on its own it would let whoever
/// answers this request nominate itself as the agent, and thus as the party
/// that can decrypt the workload.
async fn select_agent(
    scheduler_url: &str,
    exclude: &[String],
    remaining_replicas: u32,
    resources: &protocol::ManifestResources,
    trust: &AgentTrust,
) -> Result<CapacityOffer> {
    let mut url = reqwest::Url::parse(&format!(
        "{}/api/v1/agents/select",
        scheduler_url.trim_end_matches('/')
    ))
    .context("invalid scheduler URL")?;
    // Ask for as many candidates as there are replicas still to place, so the
    // mesh has room to spread them. The scheduler answers as soon as it has
    // that many rather than waiting out the query lifetime.
    {
        let mut query = url.query_pairs_mut();
        if !exclude.is_empty() {
            query.append_pair("exclude", &exclude.join(","));
        }
        query.append_pair("candidates", &remaining_replicas.max(1).to_string());
        query.append_pair("cpu_milli", &resources.cpu_milli.to_string());
        query.append_pair("memory_bytes", &resources.memory_bytes.to_string());
        query.append_pair("storage_bytes", &resources.storage_bytes.to_string());
        query.append_pair("capabilities", "multi-workload");
    }
    let offer = http_client()?
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json::<CapacityOffer>()
        .await?;
    offer.verify(now_secs())?;
    trust.authorize(&offer.signing_pubkey)?;
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

pub async fn apply_file(path: PathBuf, options: &ClientOptions) -> Result<String> {
    apply_file_internal(path, options, None, None).await
}

/// Applies a manifest against an explicit proxy URL list instead of reading
/// `PODMESH_PROXY_URL` from the environment.
pub async fn apply_file_with_proxy_urls(
    path: PathBuf,
    options: &ClientOptions,
    proxy_urls: String,
) -> Result<String> {
    apply_file_internal(path, options, None, Some(proxy_urls)).await
}

pub async fn apply_file_with_proxy_endpoints(
    path: PathBuf,
    options: &ClientOptions,
    proxy_endpoints: Vec<String>,
    workload_relay_auth_token: String,
    workload_relay_ca_certificates: Vec<Vec<u8>>,
) -> Result<String> {
    apply_file_internal(
        path,
        options,
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
    options: &ClientOptions,
    explicit_proxy_config: Option<(Vec<String>, String, Vec<Vec<u8>>)>,
    proxy_urls: Option<String>,
) -> Result<String> {
    let key_dir = key_dir()?;
    let trust = trust::resolve(&key_dir, options.trust_any_agent)?;
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
    // Loaded before proxy bootstrap: a relay token is derived per tenant, so
    // the owner identity has to be known before one can be requested.
    let (owner_public, owner_private) = owner_keys(&key_dir)?;
    let namespace_id = crypto::b64_encode(&owner_public);
    let deployment_id = protocol::deployment_id(&owner_public, &workload_name);
    let mut existing_catalog = catalog::load_if_exists(&key_dir, &deployment_id)?;
    if existing_catalog.is_none() {
        let reconciliation = reconcile_workloads(options).await?;
        existing_catalog = catalog::load_if_exists(&key_dir, &deployment_id)?;
        if existing_catalog.is_none()
            && (!reconciliation.unreachable_agents.is_empty()
                || !reconciliation.unreachable_schedulers.is_empty())
        {
            anyhow::bail!(
                "cannot safely create deployment while workload reconciliation is partial"
            );
        }
    }
    if let Some(existing) = &existing_catalog {
        anyhow::ensure!(
            existing.replica_count == replica_count,
            "scaling an existing deployment is not supported in the MVP (recorded {}, requested {})",
            existing.replica_count,
            replica_count
        );
    }
    let (manifest, resources) = protocol::validate_and_measure_manifest(&manifest)?;
    let resources = resources.with_default_sidecar()?;
    let revision_id = protocol::revision_id(&manifest);
    if existing_catalog.as_ref().is_some_and(|catalog| {
        catalog.replicas.len() == replica_count as usize
            && catalog.replicas.iter().all(|replica| {
                replica.revision_id == revision_id && !service_mesh_renewal_due(replica, now_secs())
            })
    }) {
        return Ok(deployment_id);
    }
    let annotations = protocol::PodmeshAnnotations::from_manifest_yaml(
        std::str::from_utf8(&manifest).context("manifest is not UTF-8")?,
    )?;
    let (encoded_proxy_endpoints, workload_relay_auth_token, workload_relay_ca_certificates) =
        if let Some(explicit) = explicit_proxy_config {
            explicit
        } else if let Some(proxy_urls) = proxy_urls.as_deref() {
            bootstrap_from_proxies(proxy_urls, &namespace_id, &key_dir).await?
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
    let (response_kem_public, response_kem_private) = response_keys(&key_dir)?;
    // The owner authorizes each proxy explicitly. Without a grant a proxy will
    // not answer this tenant's sidecars, so it is provisioned as part of the
    // deployment rather than as a separate manual step.
    if let Some(proxy_urls) = proxy_urls.as_deref() {
        grant_proxies(proxy_urls, &owner_public, &owner_private).await?;
    }
    let api_base = resolve_api_base(options.api_base());

    let mut catalog = if let Some(mut catalog) = existing_catalog {
        let mut failures = Vec::new();
        for placement_index in 0..catalog.replicas.len() {
            if catalog.replicas[placement_index].revision_id == revision_id
                && !service_mesh_renewal_due(&catalog.replicas[placement_index], now_secs())
            {
                catalog.replicas[placement_index].update_state = ReplicaUpdateState::Active;
                continue;
            }
            catalog.replicas[placement_index].update_state = ReplicaUpdateState::Updating {
                requested_revision_id: revision_id.clone(),
            };
            catalog::save(&key_dir, &catalog)?;
            let placement = catalog.replicas[placement_index].clone();
            match update_replica(
                ReplicaRequest {
                    api_base: &api_base,
                    namespace_id: &namespace_id,
                    workload_name: &workload_name,
                    manifest: &manifest,
                    revision_id: &revision_id,
                    replica_index: placement.replica_index,
                    replica_count,
                    proxy_endpoints: &proxy_endpoints,
                    workload_relay_auth_token: &workload_relay_auth_token,
                    workload_relay_ca_certificates: &workload_relay_ca_certificates,
                    resources: &resources,
                },
                &placement,
                OwnerSigningKeys {
                    public: &owner_public,
                    private: &owner_private,
                },
                ResponseKemKeys {
                    public: &response_kem_public,
                    private: &response_kem_private,
                },
            )
            .await
            {
                Ok(updated) => {
                    catalog.replicas[placement_index] = updated;
                    catalog::save(&key_dir, &catalog)?;
                }
                Err(error) => failures.push(format!(
                    "replica {} on {}: {error:#}",
                    placement.replica_index, placement.agent_endpoint_id
                )),
            }
        }
        if !failures.is_empty() {
            return Err(anyhow!(
                "deployment update was only partially successful: {}",
                failures.join("; ")
            ));
        }
        catalog
    } else {
        DeploymentCatalog::new(deployment_id.clone(), workload_name.clone(), replica_count)
    };

    // `podctl` places the replicas itself: for every replica it asks a
    // scheduler for one agent, excluding the agents this deployment already
    // occupies, then admits and deploys directly against that agent. The
    // scheduler never decides how many replicas exist or where they go.
    // The catalog is written after every replica rather than once at the end.
    // A deployment that fails on its third replica still leaves the first two
    // running, and those replicas are only reachable if their placement was
    // already recorded.
    let mut occupied_agents: Vec<String> = catalog
        .replicas
        .iter()
        .map(|replica| replica.agent_endpoint_id.clone())
        .collect();
    for replica_index in 0..replica_count {
        if catalog
            .replicas
            .iter()
            .any(|replica| replica.replica_index == replica_index)
        {
            continue;
        }
        let mut excluded_agents = occupied_agents.clone();
        let mut capacity_refusals = Vec::new();
        let (agent_endpoint_id, placement) = loop {
            let agent = select_agent(
                &api_base,
                &excluded_agents,
                replica_count.saturating_sub(replica_index),
                &resources,
                &trust,
            )
            .await
            .with_context(|| {
                if capacity_refusals.is_empty() {
                    format!(
                        "no agent available for replica {} of {replica_count}; \
                         each replica needs its own agent",
                        replica_index + 1
                    )
                } else {
                    format!(
                        "no replacement agent available for replica {} of {replica_count} after capacity refusals from {}",
                        replica_index + 1,
                        capacity_refusals.join(", ")
                    )
                }
            })?;
            let agent_endpoint_id = agent_endpoint_id(&agent)?;
            anyhow::ensure!(
                !excluded_agents.contains(&agent_endpoint_id),
                "scheduler offered excluded agent {agent_endpoint_id}"
            );
            let result = deploy_replica(
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
                OwnerSigningKeys {
                    public: &owner_public,
                    private: &owner_private,
                },
                ResponseKemKeys {
                    public: &response_kem_public,
                    private: &response_kem_private,
                },
            )
            .await;
            match result {
                Ok(placement) => break (agent_endpoint_id, placement),
                Err(error) => {
                    let Some(refusal) = error.downcast_ref::<AdmissionCapacityRefusal>() else {
                        return Err(error).with_context(|| {
                            format!("deploying replica {replica_index} to {agent_endpoint_id}")
                        });
                    };
                    capacity_refusals.push(format!("{agent_endpoint_id} ({})", refusal.reason));
                    excluded_agents.push(agent_endpoint_id);
                    if capacity_refusals.len() > MAX_ADMISSION_CAPACITY_RETRIES {
                        return Err(anyhow!(
                            "replica {} exhausted {} admission capacity retries: {}",
                            replica_index + 1,
                            MAX_ADMISSION_CAPACITY_RETRIES,
                            capacity_refusals.join(", ")
                        ));
                    }
                }
            }
        };
        occupied_agents.push(agent_endpoint_id);
        catalog.replicas.push(placement);
        catalog
            .replicas
            .sort_by_key(|placement| placement.replica_index);
        catalog::save(&key_dir, &catalog)?;
    }

    Ok(deployment_id)
}

/// Deployment inputs and placement identity for one replica.
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

#[derive(Clone, Copy)]
struct OwnerSigningKeys<'a> {
    public: &'a [u8],
    private: &'a [u8],
}

struct ResponseKemKeys<'a> {
    public: &'a [u8],
    private: &'a [u8],
}

struct PreparedExecution {
    capsule: EncryptedWorkloadCapsule,
    credential_expires_at_secs: u64,
}

impl ReplicaRequest<'_> {
    fn prepare_execution(
        &self,
        owner: OwnerSigningKeys<'_>,
        agent_kem_pubkey: &str,
        now: u64,
    ) -> Result<PreparedExecution> {
        let credential_expires_at_secs = now + WORKLOAD_CREDENTIAL_TTL_SECS;
        let credential = protocol::mint_workload_credential(
            owner.private,
            owner.public,
            &protocol::WorkloadCredentialClaims {
                tenant_owner: self.namespace_id.to_string(),
                manifest_id: protocol::route_id(owner.public, self.workload_name),
                issued_at_secs: now,
                expires_at_secs: credential_expires_at_secs,
                token_id: uuid::Uuid::new_v4().to_string(),
            },
            now,
        )
        .context("mint workload credential")?;
        let execution = ExecutionSpec {
            workload_name: self.workload_name.to_string(),
            replica_index: self.replica_index,
            replica_count: self.replica_count,
            manifest: self.manifest.to_vec(),
            proxy_endpoints: self.proxy_endpoints.to_vec(),
            workload_credential_b64: protocol::workload_credential_to_b64(&credential),
            workload_relay_auth_token: self.workload_relay_auth_token.to_string(),
            workload_relay_ca_certificates: self.workload_relay_ca_certificates.to_vec(),
        };
        let execution_bytes = postcard::to_allocvec(&execution)?;
        let mut dek = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut dek);
        let (ciphertext, nonce) = crypto::encrypt_payload_with_key(&dek, &execution_bytes)?;
        let agent_kem = crypto::b64_decode(agent_kem_pubkey)?;
        Ok(PreparedExecution {
            capsule: EncryptedWorkloadCapsule {
                ciphertext,
                nonce: nonce.to_vec(),
                wrapped_dek: crypto::encrypt_payload_for_recipient(&agent_kem, &dek)?,
            },
            credential_expires_at_secs,
        })
    }
}

async fn update_replica(
    request: ReplicaRequest<'_>,
    placement: &ReplicaPlacement,
    owner_keys: OwnerSigningKeys<'_>,
    response_kem_keys: ResponseKemKeys<'_>,
) -> Result<ReplicaPlacement> {
    let OwnerSigningKeys {
        public: owner_public,
        private: owner_private,
    } = owner_keys;
    let ResponseKemKeys {
        public: response_kem_public,
        private: response_kem_private,
    } = response_kem_keys;
    let workload_id =
        protocol::workload_id(owner_public, request.workload_name, request.replica_index);
    anyhow::ensure!(
        workload_id == placement.receipt.workload_id,
        "catalog workload identity does not match requested update"
    );

    let prepared =
        request.prepare_execution(owner_keys, &placement.agent_kem_pubkey, now_secs())?;
    let issued_at_secs = now_secs();
    let request_id = uuid::Uuid::new_v4().to_string();
    let grant = DeploymentGrant {
        version: AGENT_PROTOCOL_VERSION,
        namespace_id: request.namespace_id.to_string(),
        workload_id: workload_id.clone(),
        revision_id: request.revision_id.to_string(),
        target_node_id: placement.agent_signing_pubkey.clone(),
        response_kem_pubkey: crypto::b64_encode(response_kem_public),
        reservation_id: request_id.clone(),
        capsule: prepared.capsule,
        issued_at_secs,
        expires_at_secs: issued_at_secs + REQUEST_TTL_SECS,
        nonce: uuid::Uuid::new_v4().to_string(),
        owner_signature: String::new(),
    }
    .sign(owner_private)?;
    let update = UpdateRequest {
        version: AGENT_PROTOCOL_VERSION,
        request_id,
        namespace_id: request.namespace_id.to_string(),
        workload_id: workload_id.clone(),
        target_node_id: placement.agent_signing_pubkey.clone(),
        response_kem_pubkey: crypto::b64_encode(response_kem_public),
        expected_revision_id: placement.revision_id.clone(),
        requested_revision_id: request.revision_id.to_string(),
        refresh_service_mesh: placement.revision_id == request.revision_id,
        cpu_milli: request.resources.cpu_milli,
        memory_bytes: request.resources.memory_bytes,
        storage_bytes: request.resources.storage_bytes,
        grant,
        issued_at_secs,
        expires_at_secs: issued_at_secs + REQUEST_TTL_SECS,
        nonce: uuid::Uuid::new_v4().to_string(),
        owner_signature: String::new(),
    }
    .sign(owner_private)?;
    let response_body = post_encrypted(
        request.api_base,
        &placement.agent_endpoint_id,
        "update",
        encrypt_for(&update, &placement.agent_kem_pubkey)?,
    )
    .await?;
    let response: UpdateResponse = decrypt_from(&response_body, response_kem_private)?;
    response.verify(now_secs())?;
    anyhow::ensure!(
        response.request_id == update.request_id
            && response.expected_revision_id == update.expected_revision_id
            && response.requested_revision_id == update.requested_revision_id
            && response.receipt.namespace_id == request.namespace_id
            && response.receipt.workload_id == workload_id
            && response.receipt.agent_node_id == placement.agent_signing_pubkey,
        "update response binding mismatch"
    );
    match response.outcome {
        UpdateOutcome::Applied | UpdateOutcome::AlreadyActive => {
            anyhow::ensure!(
                response.receipt.revision_id == request.revision_id,
                "update response confirmed an unexpected revision"
            );
        }
        UpdateOutcome::Conflict => {
            return Err(anyhow!(
                "revision conflict: agent reports active revision {}",
                response.receipt.revision_id
            ));
        }
        UpdateOutcome::Rejected => {
            return Err(anyhow!("agent rejected update: {}", response.reason));
        }
    }
    let mut updated = placement.clone();
    updated.revision_id = response.receipt.revision_id.clone();
    updated.receipt = response.receipt;
    updated.api_base = request.api_base.to_string();
    updated.service_mesh_expires_at_secs = prepared.credential_expires_at_secs;
    updated.update_state = ReplicaUpdateState::Active;
    Ok(updated)
}

/// Admits and deploys a single replica onto one already-selected agent.
async fn deploy_replica(
    request: ReplicaRequest<'_>,
    agent: &CapacityOffer,
    agent_endpoint_id: &str,
    owner_keys: OwnerSigningKeys<'_>,
    response_kem_keys: ResponseKemKeys<'_>,
) -> Result<ReplicaPlacement> {
    let OwnerSigningKeys {
        public: owner_public,
        private: owner_private,
    } = owner_keys;
    let ResponseKemKeys {
        public: response_kem_public,
        private: response_kem_private,
    } = response_kem_keys;
    let workload_id =
        protocol::workload_id(owner_public, request.workload_name, request.replica_index);

    let issued_at_secs = now_secs();
    let admission = AdmissionRequest {
        version: AGENT_PROTOCOL_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        namespace_id: request.namespace_id.to_string(),
        workload_id: workload_id.clone(),
        target_node_id: agent.signing_pubkey.clone(),
        response_kem_pubkey: crypto::b64_encode(response_kem_public),
        cpu_milli: request.resources.cpu_milli,
        memory_bytes: request.resources.memory_bytes,
        storage_bytes: request.resources.storage_bytes,
        issued_at_secs,
        expires_at_secs: issued_at_secs + REQUEST_TTL_SECS,
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
    if !reservation.accepted {
        if is_capacity_refusal(&reservation.reason) {
            return Err(AdmissionCapacityRefusal {
                reason: reservation.reason,
            }
            .into());
        }
        return Err(anyhow!("agent rejected workload: {}", reservation.reason));
    }
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

    let prepared = request.prepare_execution(owner_keys, &agent.kem_pubkey, now_secs())?;
    let issued_at_secs = now_secs();
    let grant = DeploymentGrant {
        version: AGENT_PROTOCOL_VERSION,
        namespace_id: request.namespace_id.to_string(),
        workload_id: workload_id.clone(),
        revision_id: request.revision_id.to_string(),
        target_node_id: agent.signing_pubkey.clone(),
        response_kem_pubkey: crypto::b64_encode(response_kem_public),
        reservation_id: reservation.reservation_id,
        capsule: prepared.capsule,
        issued_at_secs,
        expires_at_secs: issued_at_secs + REQUEST_TTL_SECS,
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
        revision_id: receipt.revision_id.clone(),
        receipt,
        api_base: request.api_base.to_string(),
        agent_endpoint_id: agent_endpoint_id.to_string(),
        agent_endpoint: agent.agent_endpoint.clone(),
        agent_kem_pubkey: agent.kem_pubkey.clone(),
        agent_signing_pubkey: agent.signing_pubkey.clone(),
        service_mesh_expires_at_secs: prepared.credential_expires_at_secs,
        update_state: ReplicaUpdateState::Active,
    })
}

/// Sends one owner-signed lifecycle command to the agent holding a replica.
///
/// The command names the agent that accepted the deployment, so if a scheduler
/// later answers with a different agent the command simply will not verify
/// there. Nothing but the owner key can produce it.
async fn command(
    key_dir: &Path,
    placement: &ReplicaPlacement,
    operation: WorkloadOperation,
    tail: Option<usize>,
    api_base: Option<&str>,
) -> Result<WorkloadCommandResponse> {
    let workload_id = &placement.receipt.workload_id;
    let (owner_public, owner_private) = owner_keys(key_dir)?;
    let (response_kem_public, response_kem_private) = response_keys(key_dir)?;
    let issued_at_secs = now_secs();
    let command = WorkloadCommand {
        version: AGENT_PROTOCOL_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        namespace_id: crypto::b64_encode(&owner_public),
        workload_id: workload_id.clone(),
        target_node_id: placement.agent_signing_pubkey.clone(),
        operation,
        log_tail: tail.map(|value| value.min(10_000) as u32),
        response_kem_pubkey: crypto::b64_encode(&response_kem_public),
        issued_at_secs,
        expires_at_secs: issued_at_secs + REQUEST_TTL_SECS,
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
        response.request_id == command.request_id
            && &response.workload_id == workload_id
            && response.agent_node_id == placement.agent_signing_pubkey,
        "workload response binding mismatch"
    );
    Ok(response)
}

/// The outcome of one lifecycle command against one replica.
struct ReplicaOutcome {
    replica_index: u32,
    agent_endpoint_id: String,
    result: Result<WorkloadCommandResponse>,
}

/// Runs one lifecycle command against every replica of a deployment.
///
/// One unreachable agent must not hide the replicas that did answer, so every
/// replica is attempted and the per-replica outcome is reported.
async fn command_all(
    key_dir: &Path,
    identifier: &str,
    operation: WorkloadOperation,
    tail: Option<usize>,
    api_base: Option<&str>,
) -> Result<(DeploymentCatalog, Vec<ReplicaOutcome>)> {
    let catalog = catalog::load(key_dir, identifier)?;
    let mut outcomes = Vec::with_capacity(catalog.replicas.len());
    for placement in &catalog.replicas {
        let result = command(key_dir, placement, operation, tail, api_base).await;
        outcomes.push(ReplicaOutcome {
            replica_index: placement.replica_index,
            agent_endpoint_id: placement.agent_endpoint_id.clone(),
            result,
        });
    }
    Ok((catalog, outcomes))
}

/// Renders per-replica payloads, failing only after every replica was tried so
/// the operator sees which ones are healthy.
fn render_replica_payloads(outcomes: &[ReplicaOutcome], operation: &str) -> Result<String> {
    let mut rendered = Vec::with_capacity(outcomes.len());
    let mut failures = Vec::new();
    for outcome in outcomes {
        match &outcome.result {
            Ok(response) if response.ok => rendered.push(serde_json::json!({
                "replica_index": outcome.replica_index,
                "workload_id": response.workload_id,
                "agent_endpoint_id": outcome.agent_endpoint_id,
                "payload": response.payload,
            })),
            Ok(response) => failures.push(format!(
                "replica {} on agent {}: {}",
                outcome.replica_index, outcome.agent_endpoint_id, response.payload
            )),
            Err(error) => failures.push(format!(
                "replica {} on agent {}: {error:#}",
                outcome.replica_index, outcome.agent_endpoint_id
            )),
        }
    }
    anyhow::ensure!(
        failures.is_empty(),
        "{operation} failed for {} of {} replicas:\n  {}",
        failures.len(),
        outcomes.len(),
        failures.join("\n  ")
    );
    serde_json::to_string_pretty(&rendered).map_err(Into::into)
}

/// Deletes every replica of a deployment addressed by manifest file.
pub async fn delete_file(path: PathBuf, force: bool, options: &ClientOptions) -> Result<String> {
    let (workload_name, _, _) = canonical_manifest(&path)?;
    let key_dir = key_dir()?;
    let (owner_public, _) = owner_keys(&key_dir)?;
    delete_deployment(
        &protocol::deployment_id(&owner_public, &workload_name),
        force,
        options,
    )
    .await
}

/// Deletes every replica of a deployment addressed by id or by name.
///
/// The catalog entry is dropped only once every replica confirmed. A replica
/// that could not be reached stays in the catalog so it can be retried, rather
/// than becoming an orphan with no owner-side handle.
pub async fn delete_deployment(
    identifier: &str,
    force: bool,
    options: &ClientOptions,
) -> Result<String> {
    let key_dir = key_dir()?;
    let (mut catalog, outcomes) = command_all(
        &key_dir,
        identifier,
        WorkloadOperation::Delete,
        None,
        options.api_base(),
    )
    .await?;

    let mut deleted = Vec::new();
    let mut failures = Vec::new();
    for outcome in &outcomes {
        match &outcome.result {
            Ok(response) if response.ok => deleted.push(outcome.replica_index),
            Ok(response) => failures.push(format!(
                "replica {} on agent {}: {}",
                outcome.replica_index, outcome.agent_endpoint_id, response.payload
            )),
            Err(error) => failures.push(format!(
                "replica {} on agent {}: {error:#}",
                outcome.replica_index, outcome.agent_endpoint_id
            )),
        }
    }

    if failures.is_empty() {
        catalog::remove(&key_dir, &catalog.deployment_id)?;
        return Ok(catalog.deployment_id);
    }

    // Keep the replicas that are still alive addressable.
    catalog
        .replicas
        .retain(|placement| !deleted.contains(&placement.replica_index));
    catalog::save(&key_dir, &catalog)?;
    if force {
        log::warn!(
            "--force: dropping the catalog entry for {} while {} replica(s) remain undeleted",
            catalog.deployment_id,
            catalog.replicas.len()
        );
        catalog::remove(&key_dir, &catalog.deployment_id)?;
        return Ok(catalog.deployment_id);
    }
    anyhow::bail!(
        "delete failed for {} of {} replicas; the remaining replicas stay in the catalog so \
         the command can be retried (use --force to drop them anyway):\n  {}",
        failures.len(),
        outcomes.len(),
        failures.join("\n  ")
    )
}

pub async fn get_pod(identifier: &str, options: &ClientOptions) -> Result<String> {
    let key_dir = key_dir()?;
    let (_, outcomes) = command_all(
        &key_dir,
        identifier,
        WorkloadOperation::Status,
        None,
        options.api_base(),
    )
    .await?;
    render_replica_payloads(&outcomes, "status")
}

pub async fn get_logs(
    identifier: &str,
    tail: Option<usize>,
    options: &ClientOptions,
) -> Result<String> {
    let key_dir = key_dir()?;
    let (_, outcomes) = command_all(
        &key_dir,
        identifier,
        WorkloadOperation::Logs,
        tail,
        options.api_base(),
    )
    .await?;
    render_replica_payloads(&outcomes, "logs")
}

/// One workload an agent reports for this owner.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredWorkload {
    pub agent_endpoint_id: String,
    pub agent_node_id: String,
    pub workload_id: String,
    pub workload_name: String,
    pub replica_index: u32,
    pub replica_count: u32,
    pub state: String,
    /// True when the local catalog has no record of this workload.
    ///
    /// An orphan is a workload still running and still consuming capacity that
    /// nothing local can address — the failure mode of a lost or overwritten
    /// catalog.
    pub orphaned: bool,
}

/// What the mesh reports for this owner.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveryReport {
    pub workloads: Vec<DiscoveredWorkload>,
    /// Agents that did not answer, so a caller knows the view is partial.
    pub unreachable_agents: Vec<String>,
    /// Schedulers that did not complete the mesh-wide query.
    pub unreachable_schedulers: Vec<String>,
}

#[derive(serde::Deserialize)]
struct WorkloadListEntry {
    agent_endpoint_id: String,
    response_b64: String,
}

#[derive(serde::Deserialize)]
struct WorkloadListReply {
    answered: Vec<WorkloadListEntry>,
    unreachable_agents: Vec<String>,
    unreachable_schedulers: Vec<String>,
}

/// Ask the mesh which workloads it is running for this owner.
///
/// The local catalog is the only index of where replicas were placed, so it
/// cannot answer this: a catalog that was lost, overwritten, or written on
/// another machine leaves workloads running with nothing to point at them. This
/// asks the agents instead, and marks anything the catalog does not know about.
pub async fn discover_workloads(options: &ClientOptions) -> Result<DiscoveryReport> {
    reconcile_workloads(options).await
}

async fn reconcile_workloads(options: &ClientOptions) -> Result<DiscoveryReport> {
    let key_dir = key_dir()?;
    let (owner_public, owner_private) = owner_keys(&key_dir)?;
    let (response_kem_public, response_kem_private) = response_keys(&key_dir)?;
    let issued_at_secs = now_secs();

    let request = protocol::WorkloadListRequest {
        version: AGENT_PROTOCOL_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        namespace_id: crypto::b64_encode(&owner_public),
        response_kem_pubkey: crypto::b64_encode(&response_kem_public),
        issued_at_secs,
        expires_at_secs: issued_at_secs + REQUEST_TTL_SECS,
        nonce: uuid::Uuid::new_v4().to_string(),
        owner_signature: String::new(),
    }
    .sign(&owner_private)?;

    let api_base = resolve_api_base(options.api_base());
    let reply: WorkloadListReply = http_client()?
        .post(format!(
            "{}/api/v1/workloads/list",
            api_base.trim_end_matches('/')
        ))
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(request.to_bytes()?)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // Everything the local catalog already knows about, so anything else is an
    // orphan.
    let known: std::collections::HashSet<String> = catalog::load_all(&key_dir)?
        .into_iter()
        .flat_map(|catalog| {
            catalog
                .replicas
                .into_iter()
                .map(|placement| placement.receipt.workload_id)
        })
        .collect();

    let mut workloads = Vec::new();
    let mut recovered = std::collections::HashMap::<String, catalog::DeploymentCatalog>::new();
    let mut placements = std::collections::HashSet::<(String, u32)>::new();
    for entry in reply.answered {
        let sealed = crypto::b64_decode(&entry.response_b64)?;
        let response: protocol::WorkloadListResponse =
            match decrypt_from(&sealed, &response_kem_private) {
                Ok(response) => response,
                Err(error) => {
                    log::warn!(
                        "ignoring unreadable list answer from agent {}: {error:#}",
                        entry.agent_endpoint_id
                    );
                    continue;
                }
            };
        if let Err(error) = response.verify() {
            log::warn!(
                "ignoring unsigned list answer from agent {}: {error:#}",
                entry.agent_endpoint_id
            );
            continue;
        }
        anyhow::ensure!(
            response.request_id == request.request_id,
            "agent {} answered a different list request",
            entry.agent_endpoint_id
        );
        anyhow::ensure!(
            hex::encode(&response.agent_endpoint.endpoint_id) == entry.agent_endpoint_id,
            "agent answer identity does not match authenticated scheduler attachment"
        );
        for workload in response.workloads {
            anyhow::ensure!(
                workload.receipt.namespace_id == request.namespace_id,
                "agent returned a receipt for a different namespace"
            );
            let expected_workload_id = protocol::workload_id(
                &owner_public,
                &workload.workload_name,
                workload.replica_index,
            );
            anyhow::ensure!(
                workload.workload_id == expected_workload_id,
                "agent returned a workload with an invalid stable identity"
            );
            anyhow::ensure!(
                placements.insert((workload.workload_id.clone(), workload.replica_index)),
                "multiple agents claim the same workload replica"
            );
            let deployment_id = protocol::deployment_id(&owner_public, &workload.workload_name);
            let recovered_catalog = if let Some(catalog) = recovered.get_mut(&deployment_id) {
                catalog
            } else {
                let catalog =
                    catalog::load_if_exists(&key_dir, &deployment_id)?.unwrap_or_else(|| {
                        catalog::DeploymentCatalog::new(
                            deployment_id.clone(),
                            workload.workload_name.clone(),
                            workload.replica_count,
                        )
                    });
                recovered.entry(deployment_id.clone()).or_insert(catalog)
            };
            anyhow::ensure!(
                recovered_catalog.workload_name == workload.workload_name
                    && recovered_catalog.replica_count == workload.replica_count,
                "agents disagree about deployment identity or replica count"
            );
            let placement = catalog::ReplicaPlacement {
                replica_index: workload.replica_index,
                revision_id: workload.revision_id.clone(),
                receipt: workload.receipt.clone(),
                api_base: api_base.clone(),
                agent_endpoint_id: entry.agent_endpoint_id.clone(),
                agent_endpoint: response.agent_endpoint.clone(),
                agent_kem_pubkey: response.agent_kem_pubkey.clone(),
                agent_signing_pubkey: response.agent_node_id.clone(),
                service_mesh_expires_at_secs: 0,
                update_state: catalog::ReplicaUpdateState::Active,
            };
            if let Some(existing) = recovered_catalog
                .replicas
                .iter_mut()
                .find(|existing| existing.replica_index == workload.replica_index)
            {
                *existing = placement;
            } else {
                recovered_catalog.replicas.push(placement);
            }
            workloads.push(DiscoveredWorkload {
                agent_endpoint_id: entry.agent_endpoint_id.clone(),
                agent_node_id: response.agent_node_id.clone(),
                orphaned: !known.contains(&workload.workload_id),
                workload_id: workload.workload_id,
                workload_name: workload.workload_name,
                replica_index: workload.replica_index,
                replica_count: workload.replica_count,
                state: workload.state,
            });
        }
    }
    for recovered_catalog in recovered.values_mut() {
        recovered_catalog
            .replicas
            .sort_by_key(|placement| placement.replica_index);
        catalog::save(&key_dir, recovered_catalog)?;
    }
    workloads.sort_by(|left, right| {
        left.workload_name
            .cmp(&right.workload_name)
            .then(left.replica_index.cmp(&right.replica_index))
    });
    Ok(DiscoveryReport {
        workloads,
        unreachable_agents: reply.unreachable_agents,
        unreachable_schedulers: reply.unreachable_schedulers,
    })
}

/// Render what the mesh reports for this owner.
pub async fn list_workloads(options: &ClientOptions, format: OutputFormat) -> Result<String> {
    let report = discover_workloads(options).await?;
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(&report).map_err(Into::into),
        OutputFormat::Table => {
            let mut out = String::from("NAME\tREPLICA\tSTATE\tAGENT\tORPHANED\n");
            for workload in &report.workloads {
                out.push_str(&format!(
                    "{}\t{}/{}\t{}\t{}\t{}\n",
                    workload.workload_name,
                    workload.replica_index,
                    workload.replica_count,
                    workload.state,
                    &workload.agent_endpoint_id[..16.min(workload.agent_endpoint_id.len())],
                    if workload.orphaned { "yes" } else { "no" },
                ));
            }
            if !report.unreachable_agents.is_empty() {
                out.push_str(&format!(
                    "\n{} agent(s) did not answer; this view may be incomplete\n",
                    report.unreachable_agents.len()
                ));
            }
            if !report.unreachable_schedulers.is_empty() {
                out.push_str(&format!(
                    "\n{} scheduler(s) did not answer; this view may be incomplete\n",
                    report.unreachable_schedulers.len()
                ));
            }
            Ok(out)
        }
    }
}

/// The owner identity this installation deploys under.
pub fn namespace_id() -> Result<String> {
    let (owner_public, _) = owner_keys(&key_dir()?)?;
    Ok(crypto::b64_encode(&owner_public))
}

/// How `podctl get pods` renders its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
}

impl std::str::FromStr for OutputFormat {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "table" => Ok(Self::Table),
            "json" => Ok(Self::Json),
            other => Err(anyhow!(
                "unknown output format {other:?}, expected table or json"
            )),
        }
    }
}

pub fn get_pods(format: OutputFormat) -> Result<String> {
    let catalogs = catalog::load_all(&key_dir()?)?;
    match format {
        OutputFormat::Json => {
            let rendered: Vec<_> = catalogs
                .iter()
                .map(|catalog| {
                    serde_json::json!({
                        "deployment_id": catalog.deployment_id,
                        "workload_name": catalog.workload_name,
                        "replicas": catalog
                            .replicas
                            .iter()
                            .map(|placement| serde_json::json!({
                                "replica_index": placement.replica_index,
                                "agent_endpoint_id": placement.agent_endpoint_id,
                                "agent_signing_pubkey": placement.agent_signing_pubkey,
                                "receipt": placement.receipt,
                            }))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect();
            serde_json::to_string_pretty(&rendered).map_err(Into::into)
        }
        OutputFormat::Table => Ok(render_table(&catalogs)),
    }
}

fn render_table(catalogs: &[DeploymentCatalog]) -> String {
    let mut out = String::from("NAME\tREPLICAS\tDEPLOYMENT ID\n");
    for catalog in catalogs {
        out.push_str(&format!(
            "{}\t{}\t{}\n",
            catalog.workload_name,
            catalog.replicas.len(),
            catalog.deployment_id
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{prelude::*, test_runner::RngSeed};

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            rng_seed: RngSeed::Fixed(0x504f_444d_4553_4805),
            ..ProptestConfig::default()
        })]

        #[test]
        fn prepared_execution_preserves_identity_and_recipient_binding(
            name in "[a-z][a-z0-9-]{0,30}",
            replica_index in 0u32..64,
            issued_at in 1_700_000_000u64..1_800_000_000,
        ) {
            let owner_dir = tempfile::tempdir().unwrap();
            let agent_dir = tempfile::tempdir().unwrap();
            let other_dir = tempfile::tempdir().unwrap();
            let (owner_public, owner_private) = owner_keys(owner_dir.path()).unwrap();
            let (agent_public, agent_private) = response_keys(agent_dir.path()).unwrap();
            let (_, other_private) = response_keys(other_dir.path()).unwrap();
            let namespace = crypto::b64_encode(&owner_public);
            let raw = format!("kind: Pod\nmetadata: {{name: {name}}}\nspec:\n  containers:\n    - {{name: app, image: nginx:alpine}}\n");
            let (manifest, resources) = protocol::validate_and_measure_manifest(raw.as_bytes()).unwrap();
            let revision = protocol::revision_id(&manifest);
            let certificates = vec![vec![1, 2, 3]];
            let request = ReplicaRequest {
                api_base: "http://localhost:3000",
                namespace_id: &namespace,
                workload_name: &name,
                manifest: &manifest,
                revision_id: &revision,
                replica_index,
                replica_count: 64,
                proxy_endpoints: &[],
                workload_relay_auth_token: "test-relay-token",
                workload_relay_ca_certificates: &certificates,
                resources: &resources,
            };
            let prepared = request.prepare_execution(
                OwnerSigningKeys { public: &owner_public, private: &owner_private },
                &crypto::b64_encode(&agent_public),
                issued_at,
            ).unwrap();
            let capsule = prepared.capsule;
            prop_assert!(crypto::decrypt_payload_from_recipient_blob(&capsule.wrapped_dek, &other_private).is_err());
            let dek: [u8; 32] = crypto::decrypt_payload_from_recipient_blob(&capsule.wrapped_dek, &agent_private).unwrap().try_into().unwrap();
            let plaintext = crypto::decrypt_payload_with_key(&dek, &capsule.nonce, &capsule.ciphertext).unwrap();
            let execution: ExecutionSpec = postcard::from_bytes(&plaintext).unwrap();
            prop_assert_eq!(&execution.workload_name, &name);
            prop_assert_eq!(execution.replica_index, replica_index);
            prop_assert_eq!(execution.replica_count, 64);
            prop_assert_eq!(&execution.manifest, &manifest);
            prop_assert_eq!(protocol::revision_id(&execution.manifest), revision);
            prop_assert_eq!(execution.proxy_endpoints.len(), 0);
            prop_assert_eq!(execution.workload_relay_auth_token, "test-relay-token");
            prop_assert_eq!(execution.workload_relay_ca_certificates, certificates);
            let credential = protocol::workload_credential_from_b64(&execution.workload_credential_b64).unwrap();
            let route = protocol::route_id(&owner_public, &name);
            prop_assert!(protocol::verify_workload_credential(&credential, &namespace, &route, issued_at).is_ok());
            prop_assert_eq!(prepared.credential_expires_at_secs, issued_at + WORKLOAD_CREDENTIAL_TTL_SECS);
            prop_assert!(protocol::verify_workload_credential(&credential, &namespace, &route, prepared.credential_expires_at_secs + 60).is_err());
            prop_assert!(protocol::verify_workload_credential(&credential, &namespace, "different-workload", issued_at).is_err());
            let mut tampered = capsule.ciphertext;
            tampered[0] ^= 1;
            prop_assert!(crypto::decrypt_payload_with_key(&dek, &capsule.nonce, &tampered).is_err());
        }
    }

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
