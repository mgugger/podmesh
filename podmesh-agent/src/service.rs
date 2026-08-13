use crate::{
    config::Config,
    runtime::WorkloadRuntime,
    store::{AgentStore, StoredWorkload},
};
use anyhow::{Context, Result, anyhow};
use axum::{Router, routing::get};
use protocol::{
    AGENT_PROTOCOL_VERSION, AdmissionRequest, AgentAttachmentHello, CAPACITY_PROTOCOL_VERSION,
    CapacityOffer, CapacityQuery, DeploymentGrant, DeploymentReceipt, ENDPOINT_RECORD_VERSION,
    EndpointRecord, ExecutionSpec, MachineRole, Reservation, SCHEDULER_MESH_PROTOCOL_VERSION,
    WorkloadCommand, WorkloadCommandResponse, WorkloadListRequest, WorkloadOperation,
};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

const RESERVATION_TTL_SECS: u64 = 30;
/// Outstanding reservations one namespace may hold on this agent at once.
///
/// A client places replicas one at a time, so it needs very few. The bound stops
/// a single owner from occupying every reservation slot and starving the rest.
const MAX_RESERVATIONS_PER_NAMESPACE: usize = 16;
const MAX_REPLAY_ENTRIES: usize = 16_384;
const MAX_RESERVATIONS: usize = 1_024;
const MAX_CONFIGURED_WORKLOADS: usize = 10_000;

/// Remembers the nonces of owner-signed messages this agent has already acted on.
///
/// Eviction is FIFO rather than fail-closed. A cache that refused new entries
/// once full would let anyone who can reach the agent brick its control plane —
/// including an owner's ability to delete their own workload — with a few
/// thousand signed messages. Every message also carries a bounded lifetime
/// (`MAX_AGENT_MESSAGE_LIFETIME_SECS`), so the window an evicted nonce could be
/// replayed in is short and the cache size is a real bound, not a guess.
#[derive(Default)]
struct ReplayCache {
    seen: HashMap<String, u64>,
    order: VecDeque<String>,
}

impl ReplayCache {
    fn record(&mut self, key: String, expires_at_secs: u64, now_secs: u64) -> Result<()> {
        while let Some(oldest) = self.order.front() {
            match self.seen.get(oldest) {
                Some(expiry) if *expiry < now_secs => {
                    let key = self.order.pop_front().expect("front checked above");
                    self.seen.remove(&key);
                }
                Some(_) => break,
                None => {
                    self.order.pop_front();
                }
            }
        }
        anyhow::ensure!(!self.seen.contains_key(&key), "replayed request");
        if self.order.len() >= MAX_REPLAY_ENTRIES
            && let Some(evicted) = self.order.pop_front()
        {
            self.seen.remove(&evicted);
        }
        self.seen.insert(key.clone(), expires_at_secs);
        self.order.push_back(key);
        Ok(())
    }
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone)]
pub struct AgentService {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    signing_public: Vec<u8>,
    signing_private: Vec<u8>,
    kem_public: Vec<u8>,
    kem_private: Vec<u8>,
    runtime: Arc<dyn WorkloadRuntime>,
    store: AgentStore,
    state: Mutex<WorkloadState>,
    replay: Mutex<ReplayCache>,
}

#[derive(Default)]
struct WorkloadState {
    active: HashMap<String, StoredWorkload>,
    reservations: HashMap<String, Reservation>,
}

#[derive(Default)]
struct ResourceUsage {
    cpu_milli: u64,
    memory_bytes: u64,
    storage_bytes: u64,
}

impl ResourceUsage {
    fn add(&mut self, cpu_milli: u32, memory_bytes: u64, storage_bytes: u64) {
        self.cpu_milli = self.cpu_milli.saturating_add(u64::from(cpu_milli));
        self.memory_bytes = self.memory_bytes.saturating_add(memory_bytes);
        self.storage_bytes = self.storage_bytes.saturating_add(storage_bytes);
    }

    fn plus(&self, cpu_milli: u32, memory_bytes: u64, storage_bytes: u64) -> Self {
        let mut total = Self {
            cpu_milli: self.cpu_milli,
            memory_bytes: self.memory_bytes,
            storage_bytes: self.storage_bytes,
        };
        total.add(cpu_milli, memory_bytes, storage_bytes);
        total
    }

    fn fits_within(&self, cpu_milli: u64, memory_bytes: u64, storage_bytes: u64) -> bool {
        self.cpu_milli <= cpu_milli
            && self.memory_bytes <= memory_bytes
            && self.storage_bytes <= storage_bytes
    }
}

impl WorkloadState {
    /// Resources committed to workloads that are actually running.
    fn active_usage(&self) -> ResourceUsage {
        let mut usage = ResourceUsage::default();
        for workload in self.active.values() {
            usage.add(
                workload.cpu_milli,
                workload.memory_bytes,
                workload.storage_bytes,
            );
        }
        usage
    }

    /// Resources held by admissions that have not deployed yet.
    fn reserved_usage(&self) -> ResourceUsage {
        let mut usage = ResourceUsage::default();
        for reservation in self.reservations.values() {
            usage.add(
                reservation.cpu_milli,
                reservation.memory_bytes,
                reservation.storage_bytes,
            );
        }
        usage
    }

    fn usage(&self) -> ResourceUsage {
        let active = self.active_usage();
        let reserved = self.reserved_usage();
        active.plus(
            u32::try_from(reserved.cpu_milli).unwrap_or(u32::MAX),
            reserved.memory_bytes,
            reserved.storage_bytes,
        )
    }

    /// Outstanding reservations held by one namespace.
    fn reservations_for(&self, namespace_id: &str) -> usize {
        self.reservations
            .values()
            .filter(|reservation| reservation.namespace_id == namespace_id)
            .count()
    }

    fn contains_workload(&self, workload_id: &str) -> bool {
        self.active.contains_key(workload_id)
            || self
                .reservations
                .values()
                .any(|reservation| reservation.workload_id == workload_id)
    }
}

impl AgentService {
    pub async fn new(config: Config, runtime: Arc<dyn WorkloadRuntime>) -> Result<Self> {
        anyhow::ensure!(
            config.max_workloads > 0 && config.max_workloads <= MAX_CONFIGURED_WORKLOADS,
            "max_workloads must be between 1 and {MAX_CONFIGURED_WORKLOADS}"
        );
        let (signing_public, signing_private) =
            crypto::load_or_create_signing_keypair(&config.key_dir)
                .context("load agent signing key")?;
        let (kem_public, kem_private) =
            crypto::load_or_create_kem_keypair(&config.key_dir).context("load agent KEM key")?;
        let store = AgentStore::open(&config.state_path, kem_public.clone(), kem_private.clone())?;
        let service = Self {
            inner: Arc::new(Inner {
                config,
                signing_public,
                signing_private,
                kem_public,
                kem_private,
                runtime,
                store,
                state: Mutex::new(WorkloadState::default()),
                replay: Mutex::new(ReplayCache::default()),
            }),
        };
        service.restore().await?;
        Ok(service)
    }

    /// The agent exposes no HTTP control plane. Owner-signed admission,
    /// deployment, and lifecycle traffic arrives exclusively over the
    /// authenticated Iroh `AGENT_CONTROL_ALPN` protocol, relayed by a
    /// scheduler. Only liveness probing stays on HTTP.
    pub fn router(&self) -> Router {
        Router::new().route("/health", get(|| async { "ok" }))
    }

    pub(crate) fn attachment_hello(
        &self,
        endpoint_address: &iroh::EndpointAddr,
        now: u64,
    ) -> Result<AgentAttachmentHello> {
        let expires_at = now + protocol::scheduler_mesh::MAX_AGENT_ATTACHMENT_LIFETIME_SECS;
        let agent_endpoint = self.signed_endpoint_record(endpoint_address, now, expires_at)?;
        AgentAttachmentHello {
            version: SCHEDULER_MESH_PROTOCOL_VERSION,
            role: MachineRole::Agent,
            agent_endpoint,
            nonce: uuid::Uuid::new_v4().to_string(),
            issued_at_secs: now,
            expires_at_secs: expires_at,
            signing_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(&self.inner.signing_public, &self.inner.signing_private, now)
    }

    pub(crate) async fn capacity_offer(
        &self,
        query: &CapacityQuery,
        endpoint_address: &iroh::EndpointAddr,
        now: u64,
    ) -> Result<Option<CapacityOffer>> {
        query.verify(now)?;
        if query
            .excluded_endpoint_ids
            .iter()
            .any(|excluded| excluded == endpoint_address.id.as_bytes())
        {
            return Ok(None);
        }
        let capabilities = vec!["multi-workload".to_string()];
        if !query
            .required_capabilities
            .iter()
            .all(|required| capabilities.contains(required))
        {
            return Ok(None);
        }

        let mut state = self.inner.state.lock().await;
        state
            .reservations
            .retain(|_, reservation| reservation.expires_at_secs >= now);
        let usage = state.usage();
        let workload_slots = state.active.len().saturating_add(state.reservations.len());
        let available_cpu =
            u64::from(self.inner.config.capacity_cpu_milli).saturating_sub(usage.cpu_milli);
        let available_memory = self
            .inner
            .config
            .capacity_memory_bytes
            .saturating_sub(usage.memory_bytes);
        let available_storage = self
            .inner
            .config
            .capacity_storage_bytes
            .saturating_sub(usage.storage_bytes);
        let can_satisfy = workload_slots < self.inner.config.max_workloads
            && available_cpu >= u64::from(query.cpu_milli)
            && available_memory >= query.memory_bytes
            && available_storage >= query.storage_bytes;
        drop(state);
        if !can_satisfy {
            return Ok(None);
        }

        let expires_at = now + protocol::capacity::MAX_CAPACITY_OFFER_LIFETIME_SECS;
        let agent_endpoint = self.signed_endpoint_record(endpoint_address, now, expires_at)?;
        CapacityOffer {
            version: CAPACITY_PROTOCOL_VERSION,
            query_id: query.query_id.clone(),
            agent_endpoint,
            kem_pubkey: crypto::b64_encode(&self.inner.kem_public),
            available_cpu_milli: u32::try_from(available_cpu)
                .unwrap_or(self.inner.config.capacity_cpu_milli),
            available_memory_bytes: available_memory,
            available_storage_bytes: available_storage,
            capabilities,
            issued_at_secs: now,
            expires_at_secs: expires_at,
            signing_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(&self.inner.signing_public, &self.inner.signing_private, now)
        .map(Some)
    }

    fn signed_endpoint_record(
        &self,
        endpoint_address: &iroh::EndpointAddr,
        now: u64,
        expires_at: u64,
    ) -> Result<EndpointRecord> {
        let relay_url = endpoint_address
            .relay_urls()
            .next()
            .map(ToString::to_string);
        let direct_addresses = endpoint_address
            .ip_addrs()
            .take(protocol::MAX_ENDPOINT_DIRECT_ADDRESSES)
            .map(ToString::to_string)
            .collect();
        EndpointRecord {
            version: ENDPOINT_RECORD_VERSION,
            endpoint_id: endpoint_address.id.as_bytes().to_vec(),
            relay_url,
            direct_addresses,
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: expires_at,
            signature: String::new(),
        }
        .sign(&self.inner.signing_public, &self.inner.signing_private, now)
    }

    /// Refuse a message this agent has already acted on.
    ///
    /// The key includes the operation, so an admission and a deployment grant
    /// that happen to share a nonce inside one namespace do not collide.
    async fn check_replay(
        &self,
        operation: &str,
        namespace: &str,
        nonce: &str,
        expires_at: u64,
    ) -> Result<()> {
        self.inner.replay.lock().await.record(
            format!("{operation}:{namespace}:{nonce}"),
            expires_at,
            now_secs(),
        )
    }

    pub(crate) fn decrypt<T: for<'de> serde::Deserialize<'de>>(&self, body: &[u8]) -> Result<T> {
        let plaintext = crypto::decrypt_payload_from_recipient_blob(body, &self.inner.kem_private)?;
        postcard::from_bytes(&plaintext).map_err(Into::into)
    }

    fn encrypt<T: serde::Serialize>(&self, value: &T, recipient: &str) -> Result<Vec<u8>> {
        let recipient = crypto::b64_decode(recipient)?;
        let plaintext = postcard::to_allocvec(value)?;
        crypto::encrypt_payload_for_recipient(&recipient, &plaintext)
    }

    /// Most capacity that may be held by reservations that have not deployed.
    ///
    /// Expressed as a share of total capacity so it scales with the agent.
    fn reserved_capacity_ceiling(&self) -> ResourceUsage {
        // Multiply before dividing, in u128, so the share is exact. Dividing
        // first loses bytes at realistic capacities and would refuse a
        // reservation that is precisely at the limit.
        let share = u128::from(self.inner.config.max_reserved_capacity_percent.min(100));
        let scale = |capacity: u64| -> u64 {
            u64::try_from(u128::from(capacity) * share / 100).unwrap_or(u64::MAX)
        };
        ResourceUsage {
            cpu_milli: scale(u64::from(self.inner.config.capacity_cpu_milli)),
            memory_bytes: scale(self.inner.config.capacity_memory_bytes),
            storage_bytes: scale(self.inner.config.capacity_storage_bytes),
        }
    }

    /// CPU this agent would currently advertise as free.
    #[cfg(test)]
    pub(crate) async fn available_cpu_milli(&self) -> u64 {
        let state = self.inner.state.lock().await;
        u64::from(self.inner.config.capacity_cpu_milli).saturating_sub(state.usage().cpu_milli)
    }

    /// Base64 of this agent's application signing key, which owner-signed
    /// messages must name as their target.
    pub fn signing_pubkey_b64(&self) -> String {
        crypto::b64_encode(&self.inner.signing_public)
    }

    pub(crate) async fn admit(&self, request: AdmissionRequest) -> Result<Vec<u8>> {
        let now = now_secs();
        request.verify(now)?;
        // Every owner-signed message names the agent it is for, so a relay
        // cannot fan one signed request out across the mesh and hold capacity
        // on every agent at once.
        anyhow::ensure!(
            request.target_node_id == crypto::b64_encode(&self.inner.signing_public),
            "admission target mismatch"
        );
        self.check_replay(
            "admission",
            &request.namespace_id,
            &request.nonce,
            request.expires_at_secs,
        )
        .await?;
        let mut state = self.inner.state.lock().await;
        state
            .reservations
            .retain(|_, value| value.expires_at_secs >= now);
        let duplicate = state.contains_workload(&request.workload_id);
        let count_available = state.active.len().saturating_add(state.reservations.len())
            < self.inner.config.max_workloads;
        let reservation_available = state.reservations.len() < MAX_RESERVATIONS
            && state.reservations_for(&request.namespace_id) < MAX_RESERVATIONS_PER_NAMESPACE;

        // Total commitment, running plus pending, must fit the agent.
        let capacity_ok = state
            .usage()
            .plus(
                request.cpu_milli,
                request.memory_bytes,
                request.storage_bytes,
            )
            .fits_within(
                u64::from(self.inner.config.capacity_cpu_milli),
                self.inner.config.capacity_memory_bytes,
                self.inner.config.capacity_storage_bytes,
            );

        // Reservations alone are additionally capped below full capacity.
        //
        // A reservation costs one signed message and is never validated against
        // a real workload until deploy, so without this an unfinished admission
        // could drive the agent's advertised capacity to zero and keep it there
        // by renewing. Running workloads are not limited this way: consuming an
        // agent by actually deploying to it is what agents are for.
        let reserved_ceiling = self.reserved_capacity_ceiling();
        let reservation_capacity_ok = state
            .reserved_usage()
            .plus(
                request.cpu_milli,
                request.memory_bytes,
                request.storage_bytes,
            )
            .fits_within(
                reserved_ceiling.cpu_milli,
                reserved_ceiling.memory_bytes,
                reserved_ceiling.storage_bytes,
            );

        let accepted = !duplicate
            && count_available
            && reservation_available
            && capacity_ok
            && reservation_capacity_ok;
        let reservation = Reservation {
            version: AGENT_PROTOCOL_VERSION,
            reservation_id: uuid::Uuid::new_v4().to_string(),
            request_id: request.request_id.clone(),
            namespace_id: request.namespace_id.clone(),
            workload_id: request.workload_id.clone(),
            agent_node_id: String::new(),
            cpu_milli: request.cpu_milli,
            memory_bytes: request.memory_bytes,
            storage_bytes: request.storage_bytes,
            accepted,
            reason: if duplicate {
                "workload is already active or reserved".into()
            } else if !count_available {
                "agent workload limit reached".into()
            } else if !reservation_available {
                "agent reservation limit reached".into()
            } else if !capacity_ok {
                "insufficient capacity".into()
            } else if !reservation_capacity_ok {
                "pending reservation limit reached; retry once admissions settle".into()
            } else {
                String::new()
            },
            expires_at_secs: now + RESERVATION_TTL_SECS,
            signature: String::new(),
        }
        .sign(&self.inner.signing_public, &self.inner.signing_private)?;
        if accepted {
            state
                .reservations
                .insert(reservation.reservation_id.clone(), reservation.clone());
        }
        drop(state);
        self.encrypt(&reservation, &request.response_kem_pubkey)
    }

    fn decode_execution(&self, grant: &DeploymentGrant) -> Result<ExecutionSpec> {
        let dek = crypto::decrypt_payload_from_recipient_blob(
            &grant.capsule.wrapped_dek,
            &self.inner.kem_private,
        )?;
        let dek: [u8; 32] = dek.try_into().map_err(|_| anyhow!("invalid DEK length"))?;
        let plaintext = crypto::decrypt_payload_with_key(
            &dek,
            &grant.capsule.nonce,
            &grant.capsule.ciphertext,
        )?;
        let spec: ExecutionSpec = postcard::from_bytes(&plaintext)?;
        spec.validate()?;
        let namespace = crypto::b64_decode(&grant.namespace_id)?;
        anyhow::ensure!(
            protocol::workload_id(&namespace, &spec.workload_name, spec.replica_index)
                == grant.workload_id,
            "workload identity mismatch"
        );
        anyhow::ensure!(
            protocol::revision_id(&spec.manifest) == grant.revision_id,
            "revision identity mismatch"
        );
        Ok(spec)
    }

    pub(crate) async fn deploy(&self, grant: DeploymentGrant) -> Result<Vec<u8>> {
        let now = now_secs();
        grant.verify(now)?;
        anyhow::ensure!(
            grant.target_node_id == crypto::b64_encode(&self.inner.signing_public),
            "deployment target mismatch"
        );
        self.check_replay(
            "deploy",
            &grant.namespace_id,
            &grant.nonce,
            grant.expires_at_secs,
        )
        .await?;
        let mut state = self.inner.state.lock().await;
        // Consume and bind the reservation *before* consulting the active set.
        // Checking `active` first would leave the reservation intact only when
        // the probed workload exists, which is an oracle for whether some other
        // owner is running a given workload on this agent.
        let reservation = state
            .reservations
            .remove(&grant.reservation_id)
            .ok_or_else(|| anyhow!("reservation not found"))?;
        reservation.verify(now)?;
        anyhow::ensure!(
            reservation.accepted
                && reservation.namespace_id == grant.namespace_id
                && reservation.workload_id == grant.workload_id,
            "reservation binding mismatch"
        );
        anyhow::ensure!(
            !state.active.contains_key(&grant.workload_id),
            "workload is already active"
        );
        let execution = self.decode_execution(&grant)?;
        let manifest = crate::sidecar::inject(
            &execution.manifest,
            crate::sidecar::SidecarInjection {
                workload_id: &grant.workload_id,
                workload_name: &execution.workload_name,
                replica_index: execution.replica_index,
                replica_count: execution.replica_count,
                namespace_id: &grant.namespace_id,
                sidecar_image: &self.inner.config.sidecar_image,
                proxy_endpoints: &execution.proxy_endpoints,
                workload_credential_b64: &execution.workload_credential_b64,
                workload_relay_auth_token: &execution.workload_relay_auth_token,
                workload_relay_ca_certificates: &execution.workload_relay_ca_certificates,
            },
        )?;
        let (manifest, measured) = protocol::validate_and_measure_manifest(&manifest)?;
        anyhow::ensure!(
            measured.cpu_milli <= reservation.cpu_milli
                && measured.memory_bytes <= reservation.memory_bytes
                && measured.storage_bytes <= reservation.storage_bytes,
            "workload resource limits exceed signed reservation"
        );
        let mut stored = StoredWorkload {
            grant: grant.clone(),
            runtime_id: String::new(),
            deleting: false,
            cpu_milli: reservation.cpu_milli,
            memory_bytes: reservation.memory_bytes,
            storage_bytes: reservation.storage_bytes,
            workload_name: execution.workload_name.clone(),
            replica_index: execution.replica_index,
            replica_count: execution.replica_count,
        };
        self.inner.store.save(&stored)?;
        state
            .active
            .insert(grant.workload_id.clone(), stored.clone());
        drop(state);
        let runtime_id = match self
            .inner
            .runtime
            .deploy(crate::runtime::WorkloadDeployment {
                workload_id: &grant.workload_id,
                namespace_id: &grant.namespace_id,
                manifest: &manifest,
            })
            .await
        {
            Ok(runtime_id) => runtime_id,
            Err(error) => {
                self.inner.store.remove(&grant.workload_id)?;
                self.inner
                    .state
                    .lock()
                    .await
                    .active
                    .remove(&grant.workload_id);
                return Err(error);
            }
        };
        stored.runtime_id = runtime_id.clone();
        if let Err(error) = self.inner.store.save(&stored) {
            let _ = self.inner.runtime.delete(&runtime_id).await;
            self.inner.store.remove(&grant.workload_id)?;
            self.inner
                .state
                .lock()
                .await
                .active
                .remove(&grant.workload_id);
            return Err(error);
        }
        self.inner
            .state
            .lock()
            .await
            .active
            .insert(grant.workload_id.clone(), stored);
        let receipt = DeploymentReceipt {
            version: AGENT_PROTOCOL_VERSION,
            namespace_id: grant.namespace_id,
            workload_id: grant.workload_id,
            revision_id: grant.revision_id,
            agent_node_id: String::new(),
            runtime_id,
            accepted_at_secs: now,
            signature: String::new(),
        }
        .sign(&self.inner.signing_public, &self.inner.signing_private)?;
        self.encrypt(&receipt, &grant.response_kem_pubkey)
    }

    /// Report the workloads this agent holds for the signing owner.
    ///
    /// `podctl` keeps the only index of where it placed replicas, so a lost or
    /// stale catalog would otherwise leave workloads running with no way to
    /// find them. Only workloads belonging to the signing key are reported, so
    /// this discloses nothing about co-tenants.
    ///
    /// The state reported is the agent's own record, not a live runtime probe:
    /// listing is for finding workloads, and probing every pod would turn one
    /// list into one runtime call per workload.
    pub(crate) async fn list(&self, request: WorkloadListRequest) -> Result<Vec<u8>> {
        let now = now_secs();
        request.verify(now)?;
        self.check_replay(
            "list",
            &request.namespace_id,
            &request.nonce,
            request.expires_at_secs,
        )
        .await?;

        let workloads: Vec<protocol::WorkloadSummary> = self
            .inner
            .state
            .lock()
            .await
            .active
            .values()
            .filter(|stored| stored.grant.namespace_id == request.namespace_id)
            .map(|stored| protocol::WorkloadSummary {
                workload_id: stored.grant.workload_id.clone(),
                workload_name: stored.workload_name.clone(),
                revision_id: stored.grant.revision_id.clone(),
                replica_index: stored.replica_index,
                replica_count: stored.replica_count,
                state: if stored.deleting {
                    "deleting".into()
                } else if stored.runtime_id.is_empty() {
                    "starting".into()
                } else {
                    "deployed".into()
                },
                deploying_since_secs: stored.grant.issued_at_secs,
            })
            .collect();

        let response = protocol::WorkloadListResponse {
            version: AGENT_PROTOCOL_VERSION,
            request_id: request.request_id,
            agent_node_id: String::new(),
            workloads,
            responded_at_secs: now,
            signature: String::new(),
        }
        .sign(&self.inner.signing_public, &self.inner.signing_private)?;
        self.encrypt(&response, &request.response_kem_pubkey)
    }

    pub(crate) async fn command(&self, command: WorkloadCommand) -> Result<Vec<u8>> {
        let now = now_secs();
        command.verify(now)?;
        anyhow::ensure!(
            command.target_node_id == crypto::b64_encode(&self.inner.signing_public),
            "command target mismatch"
        );
        self.check_replay(
            "command",
            &command.namespace_id,
            &command.nonce,
            command.expires_at_secs,
        )
        .await?;
        // "not found" and "owned by somebody else" must be indistinguishable
        // to the caller, otherwise a lifecycle command becomes an oracle for
        // which workloads a co-tenant is running here. The distinction is only
        // written to this agent's own log.
        let active = self
            .inner
            .state
            .lock()
            .await
            .active
            .get(&command.workload_id)
            .filter(|active| active.grant.namespace_id == command.namespace_id)
            .cloned()
            .ok_or_else(|| {
                log::debug!(
                    "lifecycle command for workload {} refused: not present or not owned by the sender",
                    command.workload_id
                );
                anyhow!("workload not found")
            })?;
        let result = match command.operation {
            _ if active.deleting => Err(anyhow!("workload is deleting")),
            _ if active.runtime_id.is_empty() => Err(anyhow!("workload is starting")),
            WorkloadOperation::Status => self.inner.runtime.status(&active.runtime_id).await,
            WorkloadOperation::Logs => {
                self.inner
                    .runtime
                    .logs(&active.runtime_id, command.log_tail.unwrap_or(100))
                    .await
            }
            WorkloadOperation::Delete => {
                let deleting = {
                    let mut state = self.inner.state.lock().await;
                    let current = state
                        .active
                        .get_mut(&command.workload_id)
                        .ok_or_else(|| anyhow!("workload not found"))?;
                    anyhow::ensure!(!current.deleting, "workload is deleting");
                    current.deleting = true;
                    current.clone()
                };
                if let Err(error) = self.inner.store.save(&deleting) {
                    if let Some(current) = self
                        .inner
                        .state
                        .lock()
                        .await
                        .active
                        .get_mut(&command.workload_id)
                    {
                        current.deleting = false;
                    }
                    return Err(error);
                }
                match self.inner.runtime.delete(&active.runtime_id).await {
                    Ok(()) => {
                        self.inner.store.remove(&command.workload_id)?;
                        self.inner
                            .state
                            .lock()
                            .await
                            .active
                            .remove(&command.workload_id);
                        Ok("deleted".into())
                    }
                    Err(error) => {
                        self.inner.store.save(&active)?;
                        self.inner
                            .state
                            .lock()
                            .await
                            .active
                            .insert(command.workload_id.clone(), active);
                        Err(error)
                    }
                }
            }
        };
        let (ok, payload) = match result {
            Ok(payload) => (true, payload),
            Err(error) => (false, error.to_string()),
        };
        let response = WorkloadCommandResponse {
            version: AGENT_PROTOCOL_VERSION,
            request_id: command.request_id,
            workload_id: command.workload_id,
            agent_node_id: String::new(),
            ok,
            payload,
            responded_at_secs: now,
            signature: String::new(),
        }
        .sign(&self.inner.signing_public, &self.inner.signing_private)?;
        self.encrypt(&response, &command.response_kem_pubkey)
    }

    /// Rebuild in-memory state from the encrypted store on startup.
    ///
    /// One bad record must not take the whole agent down with it: the spec
    /// promise is that failing one workload never affects another, and an agent
    /// that refuses to start would take every co-tenant offline. A record that
    /// cannot be reconciled is therefore logged and skipped, and its resources
    /// are released rather than being held by a workload that is not running.
    async fn restore(&self) -> Result<()> {
        let workloads = self.inner.store.load_all()?;
        anyhow::ensure!(
            workloads.len() <= self.inner.config.max_workloads,
            "persisted workload count exceeds configured maximum"
        );
        let mut active = HashMap::with_capacity(workloads.len());
        for stored in workloads {
            let workload_id = stored.grant.workload_id.clone();
            match self.restore_one(stored).await {
                Ok(Some(restored)) => {
                    if active.insert(workload_id.clone(), restored).is_some() {
                        log::error!("duplicate persisted workload {workload_id}; keeping the last");
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    log::error!(
                        "workload {workload_id} could not be restored and was dropped: {error:#}"
                    );
                    if let Err(error) = self.inner.store.remove(&workload_id) {
                        log::error!("removing unrestorable workload {workload_id}: {error:#}");
                    }
                }
            }
        }
        let restored = WorkloadState {
            active,
            // Reservations are intentionally not persisted: an in-flight grant
            // whose agent restarted has no capacity held for it, and letting it
            // deploy afterwards would also make the grant replayable.
            reservations: HashMap::new(),
        };
        let usage = restored.usage();
        anyhow::ensure!(
            usage.cpu_milli <= u64::from(self.inner.config.capacity_cpu_milli)
                && usage.memory_bytes <= self.inner.config.capacity_memory_bytes
                && usage.storage_bytes <= self.inner.config.capacity_storage_bytes,
            "persisted workloads exceed configured resource capacity"
        );
        *self.inner.state.lock().await = restored;
        Ok(())
    }

    /// Reconcile a single persisted workload. `Ok(None)` means the record was
    /// a pending deletion and is now gone.
    async fn restore_one(&self, mut stored: StoredWorkload) -> Result<Option<StoredWorkload>> {
        let workload_id = stored.grant.workload_id.clone();
        if stored.deleting {
            if !stored.runtime_id.is_empty() {
                self.inner.runtime.delete(&stored.runtime_id).await?;
            }
            self.inner.store.remove(&workload_id)?;
            return Ok(None);
        }
        if !stored.runtime_id.is_empty()
            && self.inner.runtime.status(&stored.runtime_id).await.is_ok()
        {
            return Ok(Some(stored));
        }

        let execution = self
            .decode_execution(&stored.grant)
            .context("decrypt persisted workload for restart")?;
        let manifest = crate::sidecar::inject(
            &execution.manifest,
            crate::sidecar::SidecarInjection {
                workload_id: &workload_id,
                workload_name: &execution.workload_name,
                replica_index: execution.replica_index,
                replica_count: execution.replica_count,
                namespace_id: &stored.grant.namespace_id,
                sidecar_image: &self.inner.config.sidecar_image,
                proxy_endpoints: &execution.proxy_endpoints,
                workload_credential_b64: &execution.workload_credential_b64,
                workload_relay_auth_token: &execution.workload_relay_auth_token,
                workload_relay_ca_certificates: &execution.workload_relay_ca_certificates,
            },
        )?;
        let (manifest, measured) = protocol::validate_and_measure_manifest(&manifest)?;
        anyhow::ensure!(
            measured.cpu_milli <= stored.cpu_milli
                && measured.memory_bytes <= stored.memory_bytes
                && measured.storage_bytes <= stored.storage_bytes,
            "persisted workload resource limits exceed reservation"
        );
        stored.runtime_id = self
            .inner
            .runtime
            .deploy(crate::runtime::WorkloadDeployment {
                workload_id: &workload_id,
                namespace_id: &stored.grant.namespace_id,
                manifest: &manifest,
            })
            .await?;
        self.inner.store.save(&stored)?;
        Ok(Some(stored))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::RuntimeKind, runtime::MockRuntime};
    use rand::RngCore;
    use serial_test::serial;
    use std::path::PathBuf;

    /// The credential podctl would have minted for this workload.
    fn test_workload_credential(owner_public: &[u8], owner_private: &[u8], name: &str) -> String {
        let now = now_secs();
        let encoded = protocol::mint_workload_credential(
            owner_private,
            owner_public,
            &protocol::WorkloadCredentialClaims {
                tenant_owner: crypto::b64_encode(owner_public),
                manifest_id: protocol::route_id(owner_public, name),
                issued_at_secs: now,
                expires_at_secs: now + 3600,
                token_id: format!("credential-{name}"),
            },
            now,
        )
        .expect("mint test workload credential");
        protocol::workload_credential_to_b64(&encoded)
    }

    fn test_proxy_endpoints() -> Vec<EndpointRecord> {
        let now = now_secs();
        let (public, private) = crypto::generate_signing_keypair();
        vec![
            EndpointRecord {
                version: ENDPOINT_RECORD_VERSION,
                endpoint_id: iroh::SecretKey::generate().public().as_bytes().to_vec(),
                relay_url: Some("https://relay.example.test".into()),
                direct_addresses: vec!["127.0.0.1:4002".into()],
                signing_pubkey: String::new(),
                issued_at_secs: now,
                expires_at_secs: now + 60,
                signature: String::new(),
            }
            .sign(&public, &private, now)
            .unwrap(),
        ]
    }

    const TEST_MEMORY_BYTES: u64 = 512 * 1024 * 1024;
    const TEST_STORAGE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

    struct TestWorkload {
        workload_id: String,
        owner_public: Vec<u8>,
        owner_private: Vec<u8>,
        response_public: Vec<u8>,
        response_private: Vec<u8>,
    }

    fn signed_admission(
        target: &str,
        name: &str,
        cpu_milli: u32,
        memory_bytes: u64,
        storage_bytes: u64,
    ) -> (AdmissionRequest, Vec<u8>) {
        let (owner_public, owner_private) = crypto::generate_signing_keypair();
        let (response_public, response_private) = crypto::generate_kem_keypair();
        let request = AdmissionRequest {
            version: AGENT_PROTOCOL_VERSION,
            request_id: format!("request-{name}"),
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: protocol::workload_id(&owner_public, name, 0),
            target_node_id: target.to_string(),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            cpu_milli,
            memory_bytes,
            storage_bytes,
            issued_at_secs: now_secs(),
            expires_at_secs: now_secs() + 30,
            nonce: format!("admission-{name}"),
            owner_signature: String::new(),
        }
        .sign(&owner_private)
        .unwrap();
        (request, response_private)
    }

    fn decode_reservation(body: &[u8], response_private: &[u8]) -> Reservation {
        postcard::from_bytes(
            &crypto::decrypt_payload_from_recipient_blob(body, response_private).unwrap(),
        )
        .unwrap()
    }

    fn signed_capacity_query(query_id: &str, cpu_milli: u32, now: u64) -> CapacityQuery {
        let scheduler_transport = iroh::SecretKey::generate();
        let (scheduler_public, scheduler_private) = crypto::generate_signing_keypair();
        let reply_endpoint = EndpointRecord {
            version: ENDPOINT_RECORD_VERSION,
            endpoint_id: scheduler_transport.public().as_bytes().to_vec(),
            relay_url: None,
            direct_addresses: vec!["127.0.0.1:4000".into()],
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: now + 10,
            signature: String::new(),
        }
        .sign(&scheduler_public, &scheduler_private, now)
        .unwrap();
        CapacityQuery {
            version: CAPACITY_PROTOCOL_VERSION,
            query_id: query_id.into(),
            nonce: format!("nonce-{query_id}"),
            cpu_milli,
            memory_bytes: 100,
            storage_bytes: 100,
            required_capabilities: vec!["multi-workload".into()],
            excluded_endpoint_ids: Vec::new(),
            reply_endpoint,
            issued_at_secs: now,
            expires_at_secs: now + 10,
            signing_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(&scheduler_public, &scheduler_private, now)
        .unwrap()
    }

    async fn deploy_test_workload(
        service: &AgentService,
        name: &str,
        cpu_milli: u32,
    ) -> TestWorkload {
        let (owner_public, owner_private) = crypto::generate_signing_keypair();
        let (response_public, response_private) = crypto::generate_kem_keypair();
        let namespace_id = crypto::b64_encode(&owner_public);
        let workload_id = protocol::workload_id(&owner_public, name, 0);
        let admission = AdmissionRequest {
            version: AGENT_PROTOCOL_VERSION,
            request_id: format!("request-{name}"),
            namespace_id: namespace_id.clone(),
            workload_id: workload_id.clone(),
            target_node_id: service.signing_pubkey_b64(),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            cpu_milli,
            memory_bytes: TEST_MEMORY_BYTES,
            storage_bytes: TEST_STORAGE_BYTES,
            issued_at_secs: now_secs(),
            expires_at_secs: now_secs() + 30,
            nonce: format!("admission-{name}"),
            owner_signature: String::new(),
        }
        .sign(&owner_private)
        .unwrap();
        let reservation_body = service.admit(admission).await.unwrap();
        let reservation: Reservation = postcard::from_bytes(
            &crypto::decrypt_payload_from_recipient_blob(&reservation_body, &response_private)
                .unwrap(),
        )
        .unwrap();
        assert!(reservation.accepted, "{}", reservation.reason);

        let manifest = format!(
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: {name}\nspec:\n  containers:\n    - name: app\n      image: nginx\n"
        )
        .into_bytes();
        let execution = ExecutionSpec {
            workload_name: name.to_string(),
            replica_index: 0,
            replica_count: 1,
            manifest: manifest.clone(),
            proxy_endpoints: test_proxy_endpoints(),
            workload_credential_b64: test_workload_credential(&owner_public, &owner_private, name),
            workload_relay_auth_token: "r".repeat(32),
            workload_relay_ca_certificates: Vec::new(),
        };
        let mut dek = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut dek);
        let (ciphertext, nonce) =
            crypto::encrypt_payload_with_key(&dek, &postcard::to_allocvec(&execution).unwrap())
                .unwrap();
        let grant = DeploymentGrant {
            version: AGENT_PROTOCOL_VERSION,
            namespace_id,
            workload_id: workload_id.clone(),
            revision_id: protocol::revision_id(&manifest),
            target_node_id: crypto::b64_encode(&service.inner.signing_public),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            reservation_id: reservation.reservation_id,
            capsule: protocol::EncryptedWorkloadCapsule {
                ciphertext,
                nonce: nonce.to_vec(),
                wrapped_dek: crypto::encrypt_payload_for_recipient(&service.inner.kem_public, &dek)
                    .unwrap(),
            },
            issued_at_secs: now_secs(),
            expires_at_secs: now_secs() + 30,
            nonce: format!("deploy-{name}"),
            owner_signature: String::new(),
        }
        .sign(&owner_private)
        .unwrap();
        let receipt_body = service.deploy(grant).await.unwrap();
        let receipt: DeploymentReceipt = postcard::from_bytes(
            &crypto::decrypt_payload_from_recipient_blob(&receipt_body, &response_private).unwrap(),
        )
        .unwrap();
        receipt.verify().unwrap();

        TestWorkload {
            workload_id,
            owner_public,
            owner_private,
            response_public,
            response_private,
        }
    }

    async fn command_test_workload(
        service: &AgentService,
        workload: &TestWorkload,
        operation: WorkloadOperation,
        nonce: &str,
    ) -> WorkloadCommandResponse {
        let command = WorkloadCommand {
            version: AGENT_PROTOCOL_VERSION,
            request_id: nonce.to_string(),
            namespace_id: crypto::b64_encode(&workload.owner_public),
            workload_id: workload.workload_id.clone(),
            target_node_id: service.signing_pubkey_b64(),
            operation,
            log_tail: None,
            response_kem_pubkey: crypto::b64_encode(&workload.response_public),
            issued_at_secs: now_secs(),
            expires_at_secs: now_secs() + 30,
            nonce: nonce.to_string(),
            owner_signature: String::new(),
        }
        .sign(&workload.owner_private)
        .unwrap();
        let response_body = service.command(command).await.unwrap();
        postcard::from_bytes(
            &crypto::decrypt_payload_from_recipient_blob(
                &response_body,
                &workload.response_private,
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn empty_agent_offers_its_full_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let service = AgentService::new(
            Config {
                listen: "127.0.0.1:0".into(),
                key_dir: temp.path().join("keys"),
                state_path: temp.path().join("state.redb"),
                runtime: RuntimeKind::Mock,
                workload_network: "podmesh".into(),
                max_reserved_capacity_percent: crate::config::DEFAULT_MAX_RESERVED_CAPACITY_PERCENT,
                sidecar_image: "podmesh/sidecar:latest".into(),
                capacity_cpu_milli: 4_000,
                capacity_memory_bytes: 1024,
                capacity_storage_bytes: 1024,
                max_workloads: 4,
                machine: crate::machine::MachineConfig::default(),
            },
            Arc::new(MockRuntime::default()),
        )
        .await
        .unwrap();
        let now = now_secs();
        let offer = service
            .capacity_offer(
                &signed_capacity_query("empty-agent", 1_000, now),
                &test_agent_address(),
                now,
            )
            .await
            .unwrap()
            .expect("an idle agent must offer capacity");
        offer.verify(now).unwrap();
        assert_eq!(offer.available_cpu_milli, 4_000);
        assert_ne!(PathBuf::from(""), service.inner.config.key_dir);
    }

    fn test_agent_address() -> iroh::EndpointAddr {
        iroh::EndpointAddr::new(iroh::SecretKey::generate().public())
            .with_ip_addr("127.0.0.1:4100".parse().unwrap())
    }

    async fn offers_capacity(service: &AgentService) -> bool {
        let now = now_secs();
        service
            .capacity_offer(
                &signed_capacity_query(&uuid::Uuid::new_v4().to_string(), 1, now),
                &test_agent_address(),
                now,
            )
            .await
            .unwrap()
            .is_some()
    }

    /// An agent with generous capacity, on its own temporary key and state dir.
    async fn test_service(
        capacity_cpu_milli: u32,
        max_workloads: usize,
    ) -> (AgentService, tempfile::TempDir) {
        let (service, temp, _runtime) =
            test_service_with_runtime(capacity_cpu_milli, max_workloads).await;
        (service, temp)
    }

    async fn test_service_with_runtime(
        capacity_cpu_milli: u32,
        max_workloads: usize,
    ) -> (AgentService, tempfile::TempDir, Arc<MockRuntime>) {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        let service = AgentService::new(
            Config {
                listen: "127.0.0.1:0".into(),
                key_dir: temp.path().join("keys"),
                state_path: temp.path().join("state.redb"),
                runtime: RuntimeKind::Mock,
                workload_network: "podmesh".into(),
                max_reserved_capacity_percent: crate::config::DEFAULT_MAX_RESERVED_CAPACITY_PERCENT,
                sidecar_image: "podmesh/sidecar:latest".into(),
                capacity_cpu_milli,
                capacity_memory_bytes: 16 * TEST_MEMORY_BYTES,
                capacity_storage_bytes: 16 * TEST_STORAGE_BYTES,
                max_workloads,
                machine: crate::machine::MachineConfig::default(),
            },
            runtime.clone(),
        )
        .await
        .unwrap();
        (service, temp, runtime)
    }

    /// The spec requires that a non-owner lifecycle command is refused *and*
    /// that the refusal does not reveal whether the workload exists.
    #[tokio::test]
    #[serial]
    async fn a_command_signed_by_another_key_is_refused_indistinguishably() {
        let (service, _temp) = test_service(1_000, 4).await;
        let workload = deploy_test_workload(&service, "owned", 200).await;

        let (attacker_public, attacker_private) = crypto::generate_signing_keypair();
        let (attacker_kem_public, _) = crypto::generate_kem_keypair();
        let signed = |workload_id: String, nonce: &str| {
            WorkloadCommand {
                version: AGENT_PROTOCOL_VERSION,
                request_id: nonce.to_string(),
                namespace_id: crypto::b64_encode(&attacker_public),
                workload_id,
                target_node_id: service.signing_pubkey_b64(),
                operation: WorkloadOperation::Delete,
                log_tail: None,
                response_kem_pubkey: crypto::b64_encode(&attacker_kem_public),
                issued_at_secs: now_secs(),
                expires_at_secs: now_secs() + 30,
                nonce: nonce.to_string(),
                owner_signature: String::new(),
            }
            .sign(&attacker_private)
            .unwrap()
        };

        // An existing workload owned by somebody else, and one that does not
        // exist at all, must fail the same way.
        let existing = service
            .command(signed(workload.workload_id.clone(), "attack-existing"))
            .await
            .unwrap_err()
            .to_string();
        let absent = service
            .command(signed(
                protocol::workload_id(&attacker_public, "absent", 0),
                "attack-absent",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            existing, absent,
            "the refusal must not disclose whether the workload exists"
        );

        // The victim's workload is untouched.
        assert!(
            service
                .inner
                .state
                .lock()
                .await
                .active
                .contains_key(&workload.workload_id)
        );
    }

    /// An owner-signed message names one agent. Without that a relay could fan a
    /// single request out and hold capacity across the whole mesh.
    #[tokio::test]
    #[serial]
    async fn a_message_addressed_to_another_agent_is_refused() {
        let (service, _temp) = test_service(1_000, 4).await;
        let (elsewhere, _) = crypto::generate_signing_keypair();
        let (request, _) = signed_admission(
            &crypto::b64_encode(&elsewhere),
            "misaddressed",
            100,
            100,
            100,
        );
        let error = service.admit(request).await.unwrap_err().to_string();
        assert!(
            error.contains("target mismatch"),
            "unexpected error: {error}"
        );
        assert!(service.inner.state.lock().await.reservations.is_empty());
    }

    /// A payload sealed to a different agent's KEM key must be unusable, which
    /// is what makes the scheduler a blind relay.
    #[tokio::test]
    #[serial]
    async fn a_payload_encrypted_to_another_agent_cannot_be_opened() {
        let (service, _temp) = test_service(1_000, 4).await;
        let (other_kem_public, _) = crypto::generate_kem_keypair();
        let (request, _) = signed_admission(&service.signing_pubkey_b64(), "sealed", 100, 100, 100);
        let sealed = crypto::encrypt_payload_for_recipient(
            &other_kem_public,
            &postcard::to_allocvec(&request).unwrap(),
        )
        .unwrap();
        assert!(
            service.decrypt::<AdmissionRequest>(&sealed).is_err(),
            "a payload sealed to another agent must not decrypt here"
        );
    }

    /// A workload existence oracle would let anyone enumerate co-tenants: a
    /// failed deploy must consume the reservation either way.
    #[tokio::test]
    #[serial]
    async fn a_failed_deploy_does_not_reveal_whether_a_workload_exists() {
        let (service, _temp) = test_service(2_000, 8).await;
        let victim = deploy_test_workload(&service, "victim", 200).await;

        let probe = |workload_id: String, nonce: &str| {
            let (owner_public, owner_private) = crypto::generate_signing_keypair();
            let (kem_public, _) = crypto::generate_kem_keypair();
            let namespace_id = crypto::b64_encode(&owner_public);
            (
                owner_public,
                DeploymentGrant {
                    version: AGENT_PROTOCOL_VERSION,
                    namespace_id,
                    workload_id,
                    revision_id: protocol::revision_id(b"probe"),
                    target_node_id: service.signing_pubkey_b64(),
                    response_kem_pubkey: crypto::b64_encode(&kem_public),
                    reservation_id: "no-such-reservation".into(),
                    capsule: protocol::EncryptedWorkloadCapsule {
                        ciphertext: vec![1],
                        nonce: vec![0; 24],
                        wrapped_dek: vec![2],
                    },
                    issued_at_secs: now_secs(),
                    expires_at_secs: now_secs() + 30,
                    nonce: nonce.to_string(),
                    owner_signature: String::new(),
                }
                .sign(&owner_private)
                .unwrap(),
            )
        };

        let (_, existing) = probe(victim.workload_id.clone(), "probe-existing");
        let existing_error = service.deploy(existing).await.unwrap_err().to_string();
        let (attacker, _) = crypto::generate_signing_keypair();
        let (_, absent) = probe(
            protocol::workload_id(&attacker, "absent", 0),
            "probe-absent",
        );
        let absent_error = service.deploy(absent).await.unwrap_err().to_string();
        assert_eq!(
            existing_error, absent_error,
            "deploy must not distinguish an existing workload from a missing one"
        );
    }

    /// A reservation costs one signed message and is never checked against a
    /// real workload until deploy. Without a ceiling, unfinished admissions
    /// could drive advertised capacity to zero and hold it there by renewing.
    #[tokio::test]
    #[serial]
    async fn reservations_cannot_zero_out_advertised_capacity() {
        let (service, _temp) = test_service(1_000, 64).await;
        let ceiling = service.reserved_capacity_ceiling();
        assert_eq!(ceiling.cpu_milli, 500, "half of the configured capacity");

        // Reserve right up to the ceiling.
        let (request, _) = signed_admission(&service.signing_pubkey_b64(), "squat-a", 500, 1, 1);
        service.admit(request).await.unwrap();

        // The agent must still advertise capacity for somebody else.
        assert!(
            service.available_cpu_milli().await >= 500,
            "reservations must not be able to zero out availability"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_reservation_beyond_the_pending_ceiling_is_refused() {
        let (service, _temp) = test_service(1_000, 64).await;
        // Half of 1000 is the pending ceiling, so 600 cannot be reserved even
        // though the agent has 1000 free overall.
        let (request, response_key) =
            signed_admission(&service.signing_pubkey_b64(), "too-big", 600, 1, 1);
        let body = service.admit(request).await.unwrap();
        let reservation = decode_reservation(&body, &response_key);
        assert!(!reservation.accepted);
        assert!(
            reservation.reason.contains("pending reservation limit"),
            "unexpected reason: {}",
            reservation.reason
        );
    }

    /// A single owner must not be able to occupy every reservation slot.
    #[tokio::test]
    #[serial]
    async fn one_namespace_cannot_hold_every_reservation_slot() {
        let (service, _temp) = test_service(100_000, 4_096).await;
        let (owner_public, owner_private) = crypto::generate_signing_keypair();
        let (kem_public, _) = crypto::generate_kem_keypair();
        let target = service.signing_pubkey_b64();

        let mut accepted = 0usize;
        for index in 0..(MAX_RESERVATIONS_PER_NAMESPACE + 4) {
            let issued_at_secs = now_secs();
            let request = AdmissionRequest {
                version: AGENT_PROTOCOL_VERSION,
                request_id: format!("slot-{index}"),
                namespace_id: crypto::b64_encode(&owner_public),
                workload_id: protocol::workload_id(&owner_public, &format!("slot-{index}"), 0),
                target_node_id: target.clone(),
                response_kem_pubkey: crypto::b64_encode(&kem_public),
                cpu_milli: 1,
                memory_bytes: 1,
                storage_bytes: 1,
                issued_at_secs,
                expires_at_secs: issued_at_secs + 30,
                nonce: format!("slot-{index}"),
                owner_signature: String::new(),
            }
            .sign(&owner_private)
            .unwrap();
            if service.admit(request).await.is_ok() {
                accepted += 1;
            }
        }
        assert_eq!(accepted, MAX_RESERVATIONS_PER_NAMESPACE + 4);
        assert_eq!(
            service.inner.state.lock().await.reservations.len(),
            MAX_RESERVATIONS_PER_NAMESPACE,
            "one namespace must not hold more than its share of reservation slots"
        );
    }

    /// A workload whose record cannot be reconciled must not take the agent —
    /// and therefore every co-tenant — down with it.
    #[tokio::test]
    #[serial]
    async fn one_unrestorable_record_does_not_stop_the_agent_starting() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            listen: "127.0.0.1:0".into(),
            key_dir: temp.path().join("keys"),
            state_path: temp.path().join("state.redb"),
            runtime: RuntimeKind::Mock,
            workload_network: "podmesh".into(),
            max_reserved_capacity_percent: crate::config::DEFAULT_MAX_RESERVED_CAPACITY_PERCENT,
            sidecar_image: "podmesh/sidecar:latest".into(),
            capacity_cpu_milli: 4_000,
            capacity_memory_bytes: 8 * TEST_MEMORY_BYTES,
            capacity_storage_bytes: 8 * TEST_STORAGE_BYTES,
            max_workloads: 8,
            machine: crate::machine::MachineConfig::default(),
        };
        let runtime = Arc::new(MockRuntime::default());
        let service = AgentService::new(config.clone(), runtime.clone())
            .await
            .unwrap();
        let healthy = deploy_test_workload(&service, "healthy", 200).await;
        let broken = deploy_test_workload(&service, "broken", 200).await;

        // Corrupt one record's capsule so it can never be decrypted again.
        {
            let mut state = service.inner.state.lock().await;
            let stored = state.active.get_mut(&broken.workload_id).unwrap();
            stored.grant.capsule.ciphertext = vec![0xff; 32];
            stored.runtime_id.clear();
            service.inner.store.save(stored).unwrap();
        }
        drop(service);

        let restarted = AgentService::new(config, runtime)
            .await
            .expect("one bad record must not stop the agent from starting");
        let state = restarted.inner.state.lock().await;
        assert!(state.active.contains_key(&healthy.workload_id));
        assert!(
            !state.active.contains_key(&broken.workload_id),
            "the unrestorable record must be dropped, not retained"
        );
    }

    #[tokio::test]
    #[serial]
    async fn agent_handles_multiple_workloads_independently() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            listen: "127.0.0.1:0".into(),
            key_dir: temp.path().join("keys"),
            state_path: temp.path().join("state.redb"),
            runtime: RuntimeKind::Mock,
            workload_network: "podmesh".into(),
            max_reserved_capacity_percent: crate::config::DEFAULT_MAX_RESERVED_CAPACITY_PERCENT,
            sidecar_image: "podmesh/sidecar:latest".into(),
            capacity_cpu_milli: 4_000,
            capacity_memory_bytes: 2 * TEST_MEMORY_BYTES,
            capacity_storage_bytes: 2 * TEST_STORAGE_BYTES,
            max_workloads: 2,
            machine: crate::machine::MachineConfig::default(),
        };
        let service = AgentService::new(config.clone(), Arc::new(MockRuntime::default()))
            .await
            .unwrap();

        let first = deploy_test_workload(&service, "first", 400).await;
        let second = deploy_test_workload(&service, "second", 400).await;
        assert_eq!(service.inner.state.lock().await.active.len(), 2);
        assert!(!offers_capacity(&service).await);
        assert!(
            command_test_workload(&service, &first, WorkloadOperation::Status, "status-first")
                .await
                .ok
        );
        assert!(
            command_test_workload(
                &service,
                &second,
                WorkloadOperation::Status,
                "status-second"
            )
            .await
            .ok
        );

        assert!(
            command_test_workload(&service, &first, WorkloadOperation::Delete, "delete-first")
                .await
                .ok
        );
        assert_eq!(service.inner.state.lock().await.active.len(), 1);
        assert!(offers_capacity(&service).await);
        assert!(
            command_test_workload(
                &service,
                &second,
                WorkloadOperation::Status,
                "status-second-after-delete"
            )
            .await
            .ok
        );

        drop(service);
        let restored = AgentService::new(config, Arc::new(MockRuntime::default()))
            .await
            .unwrap();
        assert_eq!(restored.inner.state.lock().await.active.len(), 1);
        assert!(
            command_test_workload(
                &restored,
                &second,
                WorkloadOperation::Status,
                "status-second-after-restart"
            )
            .await
            .ok
        );
    }

    #[tokio::test]
    #[serial]
    async fn admission_rejects_aggregate_resource_overcommit() {
        let temp = tempfile::tempdir().unwrap();
        let service = AgentService::new(
            Config {
                listen: "127.0.0.1:0".into(),
                key_dir: temp.path().join("keys"),
                state_path: temp.path().join("state.redb"),
                runtime: RuntimeKind::Mock,
                workload_network: "podmesh".into(),
                max_reserved_capacity_percent: crate::config::DEFAULT_MAX_RESERVED_CAPACITY_PERCENT,
                sidecar_image: "podmesh/sidecar:latest".into(),
                capacity_cpu_milli: 4_000,
                capacity_memory_bytes: 2 * TEST_MEMORY_BYTES,
                capacity_storage_bytes: 2 * TEST_STORAGE_BYTES,
                max_workloads: 5,
                machine: crate::machine::MachineConfig::default(),
            },
            Arc::new(MockRuntime::default()),
        )
        .await
        .unwrap();
        // Two running workloads put the agent over half full. Each reservation
        // stayed under the pending ceiling on its way in; the ceiling bounds
        // admissions in flight, not what is already running.
        deploy_test_workload(&service, "large-a", 1_600).await;
        deploy_test_workload(&service, "large-b", 1_600).await;

        let (owner_public, owner_private) = crypto::generate_signing_keypair();
        let (response_public, response_private) = crypto::generate_kem_keypair();
        let request = AdmissionRequest {
            version: AGENT_PROTOCOL_VERSION,
            request_id: "overcommit-request".into(),
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: protocol::workload_id(&owner_public, "overcommit", 0),
            target_node_id: service.signing_pubkey_b64(),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            cpu_milli: 1_600,
            memory_bytes: TEST_MEMORY_BYTES,
            storage_bytes: TEST_STORAGE_BYTES,
            issued_at_secs: now_secs(),
            expires_at_secs: now_secs() + 30,
            nonce: "overcommit-admission".into(),
            owner_signature: String::new(),
        }
        .sign(&owner_private)
        .unwrap();
        let response = service.admit(request).await.unwrap();
        let reservation: Reservation = postcard::from_bytes(
            &crypto::decrypt_payload_from_recipient_blob(&response, &response_private).unwrap(),
        )
        .unwrap();
        assert!(!reservation.accepted);
        assert_eq!(reservation.reason, "insufficient capacity");
        assert_eq!(service.inner.state.lock().await.active.len(), 2);
    }

    #[tokio::test]
    #[serial]
    async fn capacity_offers_account_for_reservations_without_reserving() {
        let temp = tempfile::tempdir().unwrap();
        let service = AgentService::new(
            Config {
                listen: "127.0.0.1:0".into(),
                key_dir: temp.path().join("keys"),
                state_path: temp.path().join("state.redb"),
                runtime: RuntimeKind::Mock,
                workload_network: "podmesh".into(),
                max_reserved_capacity_percent: crate::config::DEFAULT_MAX_RESERVED_CAPACITY_PERCENT,
                sidecar_image: "podmesh/sidecar:latest".into(),
                capacity_cpu_milli: 4_000,
                capacity_memory_bytes: 1_000,
                capacity_storage_bytes: 1_000,
                max_workloads: 4,
                machine: crate::machine::MachineConfig::default(),
            },
            Arc::new(MockRuntime::default()),
        )
        .await
        .unwrap();
        let (owner_public, owner_private) = crypto::generate_signing_keypair();
        let (response_public, _) = crypto::generate_kem_keypair();
        let admission = AdmissionRequest {
            version: AGENT_PROTOCOL_VERSION,
            request_id: "reserved-capacity".into(),
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: protocol::workload_id(&owner_public, "reserved", 0),
            target_node_id: service.signing_pubkey_b64(),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            cpu_milli: 1_800,
            memory_bytes: 100,
            storage_bytes: 100,
            issued_at_secs: now_secs(),
            expires_at_secs: now_secs() + 30,
            nonce: "reserved-capacity-nonce".into(),
            owner_signature: String::new(),
        }
        .sign(&owner_private)
        .unwrap();
        service.admit(admission).await.unwrap();
        assert_eq!(service.inner.state.lock().await.reservations.len(), 1);

        let now = now_secs();
        let scheduler_transport = iroh::SecretKey::generate();
        let (scheduler_public, scheduler_private) = crypto::generate_signing_keypair();
        let reply_endpoint = EndpointRecord {
            version: ENDPOINT_RECORD_VERSION,
            endpoint_id: scheduler_transport.public().as_bytes().to_vec(),
            relay_url: None,
            direct_addresses: vec!["127.0.0.1:4000".into()],
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: now + 10,
            signature: String::new(),
        }
        .sign(&scheduler_public, &scheduler_private, now)
        .unwrap();
        let query = CapacityQuery {
            version: CAPACITY_PROTOCOL_VERSION,
            query_id: "capacity-query".into(),
            nonce: "capacity-query-nonce".into(),
            cpu_milli: 300,
            memory_bytes: 100,
            storage_bytes: 100,
            required_capabilities: vec!["multi-workload".into()],
            excluded_endpoint_ids: Vec::new(),
            reply_endpoint,
            issued_at_secs: now,
            expires_at_secs: now + 10,
            signing_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(&scheduler_public, &scheduler_private, now)
        .unwrap();
        let agent_transport = iroh::SecretKey::generate();
        let agent_address = iroh::EndpointAddr::new(agent_transport.public())
            .with_ip_addr("127.0.0.1:4100".parse().unwrap());
        let offer = service
            .capacity_offer(&query, &agent_address, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(offer.available_cpu_milli, 2_200);
        assert!(offer.expires_at_secs > query.expires_at_secs);
        assert_eq!(
            offer.expires_at_secs,
            now + protocol::capacity::MAX_CAPACITY_OFFER_LIFETIME_SECS
        );
        offer.verify(now).unwrap();
        assert_eq!(service.inner.state.lock().await.reservations.len(), 1);

        let mut oversized = query;
        oversized.query_id = "oversized-query".into();
        oversized.cpu_milli = 2_500;
        oversized = oversized
            .sign(&scheduler_public, &scheduler_private, now)
            .unwrap();
        assert!(
            service
                .capacity_offer(&oversized, &agent_address, now)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(service.inner.state.lock().await.reservations.len(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn concurrent_offers_and_admissions_never_overcommit() {
        let temp = tempfile::tempdir().unwrap();
        let service = AgentService::new(
            Config {
                listen: "127.0.0.1:0".into(),
                key_dir: temp.path().join("keys"),
                state_path: temp.path().join("state.redb"),
                runtime: RuntimeKind::Mock,
                workload_network: "podmesh".into(),
                max_reserved_capacity_percent: crate::config::DEFAULT_MAX_RESERVED_CAPACITY_PERCENT,
                sidecar_image: "podmesh/sidecar:latest".into(),
                capacity_cpu_milli: 4_000,
                capacity_memory_bytes: 1_000,
                capacity_storage_bytes: 1_000,
                max_workloads: 4,
                machine: crate::machine::MachineConfig::default(),
            },
            Arc::new(MockRuntime::default()),
        )
        .await
        .unwrap();
        let target = service.signing_pubkey_b64();
        let (first, first_response_key) = signed_admission(&target, "race-first", 1_800, 100, 100);
        let (second, second_response_key) =
            signed_admission(&target, "race-second", 1_800, 100, 100);
        let now = now_secs();
        let query = signed_capacity_query("race-query", 100, now);
        let agent_address = iroh::EndpointAddr::new(iroh::SecretKey::generate().public())
            .with_ip_addr("127.0.0.1:4100".parse().unwrap());
        let offer_futures = (0..32).map(|_| {
            let service = service.clone();
            let query = query.clone();
            let agent_address = agent_address.clone();
            async move {
                service
                    .capacity_offer(&query, &agent_address, now)
                    .await
                    .unwrap()
            }
        });
        let (offers, first_body, second_body) = tokio::join!(
            futures::future::join_all(offer_futures),
            service.admit(first),
            service.admit(second),
        );
        let reservations = [
            decode_reservation(&first_body.unwrap(), &first_response_key),
            decode_reservation(&second_body.unwrap(), &second_response_key),
        ];
        assert_eq!(
            reservations
                .iter()
                .filter(|reservation| reservation.accepted)
                .count(),
            1
        );
        assert!(offers.iter().all(|offer| {
            offer.as_ref().is_some_and(|offer| {
                offer.available_cpu_milli == 4_000 || offer.available_cpu_milli == 2_200
            })
        }));
        let state = service.inner.state.lock().await;
        assert_eq!(state.reservations.len(), 1);
        assert_eq!(state.usage().cpu_milli, 1_800);
    }

    #[tokio::test]
    #[serial]
    async fn replay_is_rejected_and_expired_reservation_releases_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let service = AgentService::new(
            Config {
                listen: "127.0.0.1:0".into(),
                key_dir: temp.path().join("keys"),
                state_path: temp.path().join("state.redb"),
                runtime: RuntimeKind::Mock,
                workload_network: "podmesh".into(),
                max_reserved_capacity_percent: crate::config::DEFAULT_MAX_RESERVED_CAPACITY_PERCENT,
                sidecar_image: "podmesh/sidecar:latest".into(),
                capacity_cpu_milli: 4_000,
                capacity_memory_bytes: 1_000,
                capacity_storage_bytes: 1_000,
                max_workloads: 4,
                machine: crate::machine::MachineConfig::default(),
            },
            Arc::new(MockRuntime::default()),
        )
        .await
        .unwrap();
        let (request, _) =
            signed_admission(&service.signing_pubkey_b64(), "replay", 1_800, 100, 100);
        service.admit(request.clone()).await.unwrap();
        assert!(service.admit(request).await.is_err());
        for reservation in service.inner.state.lock().await.reservations.values_mut() {
            reservation.expires_at_secs = 0;
        }
        let now = now_secs();
        let offer = service
            .capacity_offer(
                &signed_capacity_query("after-expiry", 1_000, now),
                &iroh::EndpointAddr::new(iroh::SecretKey::generate().public())
                    .with_ip_addr("127.0.0.1:4100".parse().unwrap()),
                now,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(offer.available_cpu_milli, 4_000);
        assert!(service.inner.state.lock().await.reservations.is_empty());
    }
}
