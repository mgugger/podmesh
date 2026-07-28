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
    WorkloadCommand, WorkloadCommandResponse, WorkloadOperation,
};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

const RESERVATION_TTL_SECS: u64 = 30;
const MAX_REPLAY_ENTRIES: usize = 16_384;
const MAX_RESERVATIONS: usize = 1_024;
const MAX_CONFIGURED_WORKLOADS: usize = 10_000;

fn now_secs() -> u64 {
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
    replay: Mutex<HashMap<String, u64>>,
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

impl WorkloadState {
    fn usage(&self) -> ResourceUsage {
        let mut usage = ResourceUsage::default();
        for workload in self.active.values() {
            usage.cpu_milli = usage
                .cpu_milli
                .saturating_add(u64::from(workload.cpu_milli));
            usage.memory_bytes = usage.memory_bytes.saturating_add(workload.memory_bytes);
            usage.storage_bytes = usage.storage_bytes.saturating_add(workload.storage_bytes);
        }
        for reservation in self.reservations.values() {
            usage.cpu_milli = usage
                .cpu_milli
                .saturating_add(u64::from(reservation.cpu_milli));
            usage.memory_bytes = usage.memory_bytes.saturating_add(reservation.memory_bytes);
            usage.storage_bytes = usage
                .storage_bytes
                .saturating_add(reservation.storage_bytes);
        }
        usage
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
        crypto::set_keypair_config(crypto::KeypairConfig {
            signing_mode: crypto::KeypairMode::Persistent,
            kem_mode: crypto::KeypairMode::Persistent,
            key_directory: Some(config.key_dir.clone()),
        });
        let (signing_public, signing_private) = crypto::ensure_keypair_on_disk()?;
        let (kem_public, kem_private) = crypto::ensure_kem_keypair_on_disk()?;
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
                replay: Mutex::new(HashMap::new()),
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

    async fn check_replay(&self, namespace: &str, nonce: &str, expires_at: u64) -> Result<()> {
        let now = now_secs();
        let mut replay = self.inner.replay.lock().await;
        replay.retain(|_, expiry| *expiry >= now);
        anyhow::ensure!(
            replay.len() < MAX_REPLAY_ENTRIES,
            "replay cache at capacity"
        );
        let key = format!("{namespace}:{nonce}");
        anyhow::ensure!(!replay.contains_key(&key), "replayed request");
        replay.insert(key, expires_at);
        Ok(())
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

    pub(crate) async fn admit(&self, request: AdmissionRequest) -> Result<Vec<u8>> {
        let now = now_secs();
        request.verify(now)?;
        self.check_replay(
            &request.namespace_id,
            &request.nonce,
            request.expires_at_secs,
        )
        .await?;
        let mut state = self.inner.state.lock().await;
        state
            .reservations
            .retain(|_, value| value.expires_at_secs >= now);
        let usage = state.usage();
        let duplicate = state.contains_workload(&request.workload_id);
        let count_available = state.active.len().saturating_add(state.reservations.len())
            < self.inner.config.max_workloads;
        let reservation_available = state.reservations.len() < MAX_RESERVATIONS;
        let capacity_ok = usage.cpu_milli.saturating_add(u64::from(request.cpu_milli))
            <= u64::from(self.inner.config.capacity_cpu_milli)
            && usage.memory_bytes.saturating_add(request.memory_bytes)
                <= self.inner.config.capacity_memory_bytes
            && usage.storage_bytes.saturating_add(request.storage_bytes)
                <= self.inner.config.capacity_storage_bytes;
        let accepted = !duplicate && count_available && reservation_available && capacity_ok;
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
        self.check_replay(&grant.namespace_id, &grant.nonce, grant.expires_at_secs)
            .await?;
        let mut state = self.inner.state.lock().await;
        anyhow::ensure!(
            !state.active.contains_key(&grant.workload_id),
            "workload is already active"
        );
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
        let execution = self.decode_execution(&grant)?;
        let manifest = crate::sidecar::inject(
            &execution.manifest,
            &grant.workload_id,
            &grant.namespace_id,
            &self.inner.config.sidecar_image,
            &execution.proxy_endpoints,
            &execution.workload_relay_auth_token,
            &execution.workload_relay_ca_certificates,
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
        };
        self.inner.store.save(&stored)?;
        state
            .active
            .insert(grant.workload_id.clone(), stored.clone());
        drop(state);
        let runtime_id = match self
            .inner
            .runtime
            .deploy(&grant.workload_id, &manifest)
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

    pub(crate) async fn command(&self, command: WorkloadCommand) -> Result<Vec<u8>> {
        let now = now_secs();
        command.verify(now)?;
        self.check_replay(
            &command.namespace_id,
            &command.nonce,
            command.expires_at_secs,
        )
        .await?;
        let active = self
            .inner
            .state
            .lock()
            .await
            .active
            .get(&command.workload_id)
            .cloned()
            .ok_or_else(|| anyhow!("workload not found"))?;
        anyhow::ensure!(
            active.grant.namespace_id == command.namespace_id
                && active.grant.workload_id == command.workload_id,
            "workload ownership mismatch"
        );
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

    async fn restore(&self) -> Result<()> {
        let workloads = self.inner.store.load_all()?;
        anyhow::ensure!(
            workloads.len() <= self.inner.config.max_workloads,
            "persisted workload count exceeds configured maximum"
        );
        let mut active = HashMap::with_capacity(workloads.len());
        for mut stored in workloads {
            let workload_id = stored.grant.workload_id.clone();
            if stored.deleting {
                if !stored.runtime_id.is_empty() {
                    self.inner.runtime.delete(&stored.runtime_id).await?;
                }
                self.inner.store.remove(&workload_id)?;
                continue;
            }
            if stored.runtime_id.is_empty()
                || self.inner.runtime.status(&stored.runtime_id).await.is_err()
            {
                let execution = self
                    .decode_execution(&stored.grant)
                    .context("decrypt persisted workload for restart")?;
                let manifest = crate::sidecar::inject(
                    &execution.manifest,
                    &workload_id,
                    &stored.grant.namespace_id,
                    &self.inner.config.sidecar_image,
                    &execution.proxy_endpoints,
                    &execution.workload_relay_auth_token,
                    &execution.workload_relay_ca_certificates,
                )?;
                let (manifest, measured) = protocol::validate_and_measure_manifest(&manifest)?;
                anyhow::ensure!(
                    measured.cpu_milli <= stored.cpu_milli
                        && measured.memory_bytes <= stored.memory_bytes
                        && measured.storage_bytes <= stored.storage_bytes,
                    "persisted workload resource limits exceed reservation"
                );
                stored.runtime_id = self.inner.runtime.deploy(&workload_id, &manifest).await?;
                self.inner.store.save(&stored)?;
            }
            anyhow::ensure!(
                active.insert(workload_id.clone(), stored).is_none(),
                "duplicate persisted workload {workload_id}"
            );
        }
        let restored = WorkloadState {
            active,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::RuntimeKind, runtime::MockRuntime};
    use rand::RngCore;
    use serial_test::serial;
    use std::path::PathBuf;

    fn test_proxy_endpoints() -> Vec<EndpointRecord> {
        let now = now_secs();
        let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
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
        name: &str,
        cpu_milli: u32,
        memory_bytes: u64,
        storage_bytes: u64,
    ) -> (AdmissionRequest, Vec<u8>) {
        let (owner_public, owner_private) = crypto::ensure_keypair_ephemeral().unwrap();
        let (response_public, response_private) =
            crypto::keypair_manager::KeypairManager::generate_fresh_keypair(
                crypto::keypair_manager::KeypairType::Kem,
            )
            .unwrap();
        let request = AdmissionRequest {
            version: AGENT_PROTOCOL_VERSION,
            request_id: format!("request-{name}"),
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: protocol::workload_id(&owner_public, name, 0),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            cpu_milli,
            memory_bytes,
            storage_bytes,
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
        let (scheduler_public, scheduler_private) = crypto::ensure_keypair_ephemeral().unwrap();
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
        let (owner_public, owner_private) = crypto::ensure_keypair_ephemeral().unwrap();
        let (response_public, response_private) =
            crypto::keypair_manager::KeypairManager::get_kem_keypair(
                crypto::keypair_manager::StorageMode::Ephemeral,
            )
            .unwrap();
        let namespace_id = crypto::b64_encode(&owner_public);
        let workload_id = protocol::workload_id(&owner_public, name, 0);
        let admission = AdmissionRequest {
            version: AGENT_PROTOCOL_VERSION,
            request_id: format!("request-{name}"),
            namespace_id: namespace_id.clone(),
            workload_id: workload_id.clone(),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            cpu_milli,
            memory_bytes: TEST_MEMORY_BYTES,
            storage_bytes: TEST_STORAGE_BYTES,
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
            operation,
            log_tail: None,
            response_kem_pubkey: crypto::b64_encode(&workload.response_public),
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
                sidecar_image: "podmesh/sidecar:latest".into(),
                capacity_cpu_milli: 1_000,
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
        assert_eq!(offer.available_cpu_milli, 1_000);
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
            sidecar_image: "podmesh/sidecar:latest".into(),
            capacity_cpu_milli: 1_000,
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
                sidecar_image: "podmesh/sidecar:latest".into(),
                capacity_cpu_milli: 1_000,
                capacity_memory_bytes: 2 * TEST_MEMORY_BYTES,
                capacity_storage_bytes: 2 * TEST_STORAGE_BYTES,
                max_workloads: 5,
                machine: crate::machine::MachineConfig::default(),
            },
            Arc::new(MockRuntime::default()),
        )
        .await
        .unwrap();
        deploy_test_workload(&service, "large", 700).await;

        let (owner_public, owner_private) = crypto::ensure_keypair_ephemeral().unwrap();
        let (response_public, response_private) =
            crypto::keypair_manager::KeypairManager::get_kem_keypair(
                crypto::keypair_manager::StorageMode::Ephemeral,
            )
            .unwrap();
        let request = AdmissionRequest {
            version: AGENT_PROTOCOL_VERSION,
            request_id: "overcommit-request".into(),
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: protocol::workload_id(&owner_public, "overcommit", 0),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            cpu_milli: 400,
            memory_bytes: TEST_MEMORY_BYTES,
            storage_bytes: TEST_STORAGE_BYTES,
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
        assert_eq!(service.inner.state.lock().await.active.len(), 1);
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
                sidecar_image: "podmesh/sidecar:latest".into(),
                capacity_cpu_milli: 1_000,
                capacity_memory_bytes: 1_000,
                capacity_storage_bytes: 1_000,
                max_workloads: 4,
                machine: crate::machine::MachineConfig::default(),
            },
            Arc::new(MockRuntime::default()),
        )
        .await
        .unwrap();
        let (owner_public, owner_private) = crypto::ensure_keypair_ephemeral().unwrap();
        let (response_public, _) = crypto::keypair_manager::KeypairManager::get_kem_keypair(
            crypto::keypair_manager::StorageMode::Ephemeral,
        )
        .unwrap();
        let admission = AdmissionRequest {
            version: AGENT_PROTOCOL_VERSION,
            request_id: "reserved-capacity".into(),
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: protocol::workload_id(&owner_public, "reserved", 0),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            cpu_milli: 600,
            memory_bytes: 100,
            storage_bytes: 100,
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
        let (scheduler_public, scheduler_private) = crypto::ensure_keypair_ephemeral().unwrap();
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
        assert_eq!(offer.available_cpu_milli, 400);
        assert!(offer.expires_at_secs > query.expires_at_secs);
        assert_eq!(
            offer.expires_at_secs,
            now + protocol::capacity::MAX_CAPACITY_OFFER_LIFETIME_SECS
        );
        offer.verify(now).unwrap();
        assert_eq!(service.inner.state.lock().await.reservations.len(), 1);

        let mut oversized = query;
        oversized.query_id = "oversized-query".into();
        oversized.cpu_milli = 500;
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
                sidecar_image: "podmesh/sidecar:latest".into(),
                capacity_cpu_milli: 1_000,
                capacity_memory_bytes: 1_000,
                capacity_storage_bytes: 1_000,
                max_workloads: 4,
                machine: crate::machine::MachineConfig::default(),
            },
            Arc::new(MockRuntime::default()),
        )
        .await
        .unwrap();
        let (first, first_response_key) = signed_admission("race-first", 600, 100, 100);
        let (second, second_response_key) = signed_admission("race-second", 600, 100, 100);
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
                offer.available_cpu_milli == 1_000 || offer.available_cpu_milli == 400
            })
        }));
        let state = service.inner.state.lock().await;
        assert_eq!(state.reservations.len(), 1);
        assert_eq!(state.usage().cpu_milli, 600);
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
                sidecar_image: "podmesh/sidecar:latest".into(),
                capacity_cpu_milli: 1_000,
                capacity_memory_bytes: 1_000,
                capacity_storage_bytes: 1_000,
                max_workloads: 4,
                machine: crate::machine::MachineConfig::default(),
            },
            Arc::new(MockRuntime::default()),
        )
        .await
        .unwrap();
        let (request, _) = signed_admission("replay", 600, 100, 100);
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
        assert_eq!(offer.available_cpu_milli, 1_000);
        assert!(service.inner.state.lock().await.reservations.is_empty());
    }
}
