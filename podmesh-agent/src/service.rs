use crate::{
    config::Config,
    runtime::WorkloadRuntime,
    store::{AgentStore, StoredWorkload},
};
use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
};
use protocol::{
    AGENT_PROTOCOL_VERSION, AdmissionRequest, AgentAdvertisement, DeploymentGrant,
    DeploymentReceipt, ExecutionSpec, Reservation, WorkloadCommand, WorkloadCommandResponse,
    WorkloadOperation,
};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

const ADVERTISEMENT_TTL_SECS: u64 = 30;
const REGISTRATION_INTERVAL: Duration = Duration::from_secs(10);
const RESERVATION_TTL_SECS: u64 = 30;
const MAX_AGENT_BODY_BYTES: usize = 20 * 1024 * 1024;
const MAX_REPLAY_ENTRIES: usize = 16_384;
const MAX_RESERVATIONS: usize = 1_024;
const MAX_CONFIGURED_WORKLOADS: usize = 10_000;

type ApiResult<T> = std::result::Result<T, (StatusCode, String)>;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn bad_request(error: impl std::fmt::Display) -> (StatusCode, String) {
    log::warn!("agent rejected request: {error}");
    (StatusCode::BAD_REQUEST, "invalid encrypted request".into())
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

fn utilization_percent(used: u64, capacity: u64) -> u8 {
    if capacity == 0 {
        return 100;
    }
    used.saturating_mul(100)
        .checked_div(capacity)
        .unwrap_or(100)
        .min(100) as u8
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

    pub fn router(&self) -> Router {
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/api/v1/advertisement", get(get_advertisement))
            .route("/api/v1/admission", post(post_admission))
            .route("/api/v1/deploy", post(post_deploy))
            .route("/api/v1/command", post(post_command))
            .layer(DefaultBodyLimit::max(MAX_AGENT_BODY_BYTES))
            .with_state(self.clone())
    }

    pub fn spawn_registration_loop(&self) -> tokio::task::JoinHandle<()> {
        let service = self.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let mut interval = tokio::time::interval(REGISTRATION_INTERVAL);
            loop {
                interval.tick().await;
                let advertisement = match service.advertisement().await {
                    Ok(value) => value,
                    Err(error) => {
                        log::error!("failed to build agent advertisement: {error}");
                        continue;
                    }
                };
                let url = format!(
                    "{}/api/v1/agents",
                    service.inner.config.scheduler_url.trim_end_matches('/')
                );
                match client.post(url).json(&advertisement).send().await {
                    Ok(response) if response.status().is_success() => {}
                    Ok(response) => log::warn!(
                        "scheduler rejected agent advertisement: {}",
                        response.status()
                    ),
                    Err(error) => log::warn!("failed to register with scheduler: {error}"),
                }
            }
        })
    }

    async fn advertisement(&self) -> Result<AgentAdvertisement> {
        let mut state = self.inner.state.lock().await;
        let now = now_secs();
        state
            .reservations
            .retain(|_, reservation| reservation.expires_at_secs >= now);
        let usage = state.usage();
        let workload_slots = state.active.len().saturating_add(state.reservations.len());
        let available = workload_slots < self.inner.config.max_workloads
            && usage.cpu_milli < u64::from(self.inner.config.capacity_cpu_milli)
            && usage.memory_bytes < self.inner.config.capacity_memory_bytes
            && usage.storage_bytes < self.inner.config.capacity_storage_bytes;
        let load_percent = [
            utilization_percent(
                usage.cpu_milli,
                u64::from(self.inner.config.capacity_cpu_milli),
            ),
            utilization_percent(usage.memory_bytes, self.inner.config.capacity_memory_bytes),
            utilization_percent(
                usage.storage_bytes,
                self.inner.config.capacity_storage_bytes,
            ),
            utilization_percent(
                workload_slots as u64,
                self.inner.config.max_workloads as u64,
            ),
        ]
        .into_iter()
        .max()
        .unwrap_or(100);
        drop(state);
        AgentAdvertisement {
            version: AGENT_PROTOCOL_VERSION,
            node_id: String::new(),
            kem_pubkey: crypto::b64_encode(&self.inner.kem_public),
            relay_url: self.inner.config.advertise_url.clone(),
            capabilities: vec!["multi-workload".into()],
            available,
            load_percent,
            expires_at_secs: now + ADVERTISEMENT_TTL_SECS,
            nonce: uuid::Uuid::new_v4().to_string(),
            signature: String::new(),
        }
        .sign(&self.inner.signing_public, &self.inner.signing_private)
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

    fn decrypt<T: for<'de> serde::Deserialize<'de>>(&self, body: &[u8]) -> Result<T> {
        let plaintext = crypto::decrypt_payload_from_recipient_blob(body, &self.inner.kem_private)?;
        postcard::from_bytes(&plaintext).map_err(Into::into)
    }

    fn encrypt<T: serde::Serialize>(&self, value: &T, recipient: &str) -> Result<Vec<u8>> {
        let recipient = crypto::b64_decode(recipient)?;
        let plaintext = postcard::to_allocvec(value)?;
        crypto::encrypt_payload_for_recipient(&recipient, &plaintext)
    }

    async fn admit(&self, request: AdmissionRequest) -> Result<Vec<u8>> {
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
            protocol::workload_id(&namespace, &spec.workload_name) == grant.workload_id,
            "workload identity mismatch"
        );
        anyhow::ensure!(
            protocol::revision_id(&spec.manifest) == grant.revision_id,
            "revision identity mismatch"
        );
        Ok(spec)
    }

    async fn deploy(&self, grant: DeploymentGrant) -> Result<Vec<u8>> {
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
            &self.inner.config.sidecar_bootstrap_peer,
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

    async fn command(&self, command: WorkloadCommand) -> Result<Vec<u8>> {
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
                    &self.inner.config.sidecar_bootstrap_peer,
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

async fn get_advertisement(
    State(service): State<AgentService>,
) -> ApiResult<axum::Json<AgentAdvertisement>> {
    service
        .advertisement()
        .await
        .map(axum::Json)
        .map_err(bad_request)
}

async fn post_admission(State(service): State<AgentService>, body: Bytes) -> ApiResult<Vec<u8>> {
    let request = service
        .decrypt::<AdmissionRequest>(&body)
        .map_err(bad_request)?;
    service.admit(request).await.map_err(bad_request)
}

async fn post_deploy(State(service): State<AgentService>, body: Bytes) -> ApiResult<Vec<u8>> {
    let grant = service
        .decrypt::<DeploymentGrant>(&body)
        .map_err(bad_request)?;
    service.deploy(grant).await.map_err(bad_request)
}

async fn post_command(State(service): State<AgentService>, body: Bytes) -> ApiResult<Vec<u8>> {
    let command = service
        .decrypt::<WorkloadCommand>(&body)
        .map_err(bad_request)?;
    service.command(command).await.map_err(bad_request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::RuntimeKind, runtime::MockRuntime};
    use rand::RngCore;
    use serial_test::serial;
    use std::path::PathBuf;

    const TEST_MEMORY_BYTES: u64 = 512 * 1024 * 1024;
    const TEST_STORAGE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

    struct TestWorkload {
        workload_id: String,
        owner_public: Vec<u8>,
        owner_private: Vec<u8>,
        response_public: Vec<u8>,
        response_private: Vec<u8>,
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
        let workload_id = protocol::workload_id(&owner_public, name);
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
            manifest: manifest.clone(),
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
    async fn empty_agent_advertises_available() {
        let temp = tempfile::tempdir().unwrap();
        let service = AgentService::new(
            Config {
                listen: "127.0.0.1:0".into(),
                advertise_url: "http://127.0.0.1:3100".into(),
                scheduler_url: "http://127.0.0.1:3000".into(),
                key_dir: temp.path().join("keys"),
                state_path: temp.path().join("state.redb"),
                runtime: RuntimeKind::Mock,
                workload_network: "podmesh".into(),
                sidecar_image: "podmesh/sidecar:latest".into(),
                sidecar_bootstrap_peer: "/dns4/proxy/udp/4002/quic-v1".into(),
                capacity_cpu_milli: 1_000,
                capacity_memory_bytes: 1024,
                capacity_storage_bytes: 1024,
                max_workloads: 4,
            },
            Arc::new(MockRuntime::default()),
        )
        .await
        .unwrap();
        let advertisement = service.advertisement().await.unwrap();
        assert!(advertisement.available);
        advertisement.verify(now_secs()).unwrap();
        assert_ne!(PathBuf::from(""), service.inner.config.key_dir);
    }

    #[tokio::test]
    #[serial]
    async fn agent_handles_multiple_workloads_independently() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            listen: "127.0.0.1:0".into(),
            advertise_url: "http://127.0.0.1:3100".into(),
            scheduler_url: "http://127.0.0.1:3000".into(),
            key_dir: temp.path().join("keys"),
            state_path: temp.path().join("state.redb"),
            runtime: RuntimeKind::Mock,
            workload_network: "podmesh".into(),
            sidecar_image: "podmesh/sidecar:latest".into(),
            sidecar_bootstrap_peer: "/dns4/proxy/udp/4002/quic-v1".into(),
            capacity_cpu_milli: 1_000,
            capacity_memory_bytes: 2 * TEST_MEMORY_BYTES,
            capacity_storage_bytes: 2 * TEST_STORAGE_BYTES,
            max_workloads: 2,
        };
        let service = AgentService::new(config.clone(), Arc::new(MockRuntime::default()))
            .await
            .unwrap();

        let first = deploy_test_workload(&service, "first", 400).await;
        let second = deploy_test_workload(&service, "second", 400).await;
        assert_eq!(service.inner.state.lock().await.active.len(), 2);
        assert!(!service.advertisement().await.unwrap().available);
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
        assert!(service.advertisement().await.unwrap().available);
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
                advertise_url: "http://127.0.0.1:3100".into(),
                scheduler_url: "http://127.0.0.1:3000".into(),
                key_dir: temp.path().join("keys"),
                state_path: temp.path().join("state.redb"),
                runtime: RuntimeKind::Mock,
                workload_network: "podmesh".into(),
                sidecar_image: "podmesh/sidecar:latest".into(),
                sidecar_bootstrap_peer: "/dns4/proxy/udp/4002/quic-v1".into(),
                capacity_cpu_milli: 1_000,
                capacity_memory_bytes: 2 * TEST_MEMORY_BYTES,
                capacity_storage_bytes: 2 * TEST_STORAGE_BYTES,
                max_workloads: 5,
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
            workload_id: protocol::workload_id(&owner_public, "overcommit"),
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
    async fn encrypted_http_lifecycle_uses_stateless_scheduler() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let scheduler_address = scheduler_listener.local_addr().unwrap();
        let scheduler = tokio::spawn(
            axum::serve(
                scheduler_listener,
                podmesh_scheduler::router(podmesh_scheduler::AgentRegistry::default()),
            )
            .into_future(),
        );

        let agent_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let agent_address = agent_listener.local_addr().unwrap();
        let agent_url = format!("http://{agent_address}");
        let service = AgentService::new(
            Config {
                listen: agent_address.to_string(),
                advertise_url: agent_url.clone(),
                scheduler_url: format!("http://{scheduler_address}"),
                key_dir: temp.path().join("keys"),
                state_path: temp.path().join("state.redb"),
                runtime: RuntimeKind::Mock,
                workload_network: "podmesh".into(),
                sidecar_image: "podmesh/sidecar:latest".into(),
                sidecar_bootstrap_peer: "/dns4/proxy/udp/4002/quic-v1".into(),
                capacity_cpu_milli: 1_000,
                capacity_memory_bytes: 1024 * 1024 * 1024,
                capacity_storage_bytes: TEST_STORAGE_BYTES,
                max_workloads: 4,
            },
            Arc::new(MockRuntime::default()),
        )
        .await
        .unwrap();
        let agent = tokio::spawn(axum::serve(agent_listener, service.router()).into_future());
        let client = reqwest::Client::new();

        let advertisement = service.advertisement().await.unwrap();
        client
            .post(format!("http://{scheduler_address}/api/v1/agents"))
            .json(&advertisement)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let selected = client
            .get(format!("http://{scheduler_address}/api/v1/agents/select"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<AgentAdvertisement>()
            .await
            .unwrap();
        assert_eq!(selected.node_id, advertisement.node_id);

        let (owner_public, owner_private) = crypto::ensure_keypair_ephemeral().unwrap();
        let (response_public, response_private) =
            crypto::keypair_manager::KeypairManager::get_kem_keypair(
                crypto::keypair_manager::StorageMode::Ephemeral,
            )
            .unwrap();
        let workload_id = protocol::workload_id(&owner_public, "demo");
        let request = AdmissionRequest {
            version: AGENT_PROTOCOL_VERSION,
            request_id: "request-1".into(),
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: workload_id.clone(),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            cpu_milli: 500,
            memory_bytes: TEST_MEMORY_BYTES,
            storage_bytes: TEST_STORAGE_BYTES,
            expires_at_secs: now_secs() + 30,
            nonce: "admission-nonce".into(),
            owner_signature: String::new(),
        }
        .sign(&owner_private)
        .unwrap();
        let admission_body = crypto::encrypt_payload_for_recipient(
            &crypto::b64_decode(&selected.kem_pubkey).unwrap(),
            &postcard::to_allocvec(&request).unwrap(),
        )
        .unwrap();
        let reservation_body = client
            .post(format!("{agent_url}/api/v1/admission"))
            .body(admission_body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let reservation: Reservation = postcard::from_bytes(
            &crypto::decrypt_payload_from_recipient_blob(&reservation_body, &response_private)
                .unwrap(),
        )
        .unwrap();
        assert!(reservation.accepted);

        let manifest = br#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"demo"},"spec":{"containers":[{"name":"app","image":"nginx"}]}}"#.to_vec();
        let execution = ExecutionSpec {
            workload_name: "demo".into(),
            manifest: manifest.clone(),
        };
        let mut dek = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut dek);
        let (ciphertext, nonce) =
            crypto::encrypt_payload_with_key(&dek, &postcard::to_allocvec(&execution).unwrap())
                .unwrap();
        let grant = DeploymentGrant {
            version: AGENT_PROTOCOL_VERSION,
            namespace_id: crypto::b64_encode(&owner_public),
            workload_id: workload_id.clone(),
            revision_id: protocol::revision_id(&manifest),
            target_node_id: selected.node_id.clone(),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            reservation_id: reservation.reservation_id,
            capsule: protocol::EncryptedWorkloadCapsule {
                ciphertext,
                nonce: nonce.to_vec(),
                wrapped_dek: crypto::encrypt_payload_for_recipient(
                    &crypto::b64_decode(&selected.kem_pubkey).unwrap(),
                    &dek,
                )
                .unwrap(),
            },
            issued_at_secs: now_secs(),
            expires_at_secs: now_secs() + 30,
            nonce: "deploy-nonce".into(),
            owner_signature: String::new(),
        }
        .sign(&owner_private)
        .unwrap();
        let deploy_body = crypto::encrypt_payload_for_recipient(
            &crypto::b64_decode(&selected.kem_pubkey).unwrap(),
            &postcard::to_allocvec(&grant).unwrap(),
        )
        .unwrap();
        let receipt_body = client
            .post(format!("{agent_url}/api/v1/deploy"))
            .body(deploy_body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let receipt: DeploymentReceipt = postcard::from_bytes(
            &crypto::decrypt_payload_from_recipient_blob(&receipt_body, &response_private).unwrap(),
        )
        .unwrap();
        receipt.verify().unwrap();

        for (operation, nonce) in [
            (WorkloadOperation::Status, "status-nonce"),
            (WorkloadOperation::Delete, "delete-nonce"),
        ] {
            let command = WorkloadCommand {
                version: AGENT_PROTOCOL_VERSION,
                request_id: nonce.into(),
                namespace_id: crypto::b64_encode(&owner_public),
                workload_id: workload_id.clone(),
                operation,
                log_tail: None,
                response_kem_pubkey: crypto::b64_encode(&response_public),
                expires_at_secs: now_secs() + 30,
                nonce: nonce.into(),
                owner_signature: String::new(),
            }
            .sign(&owner_private)
            .unwrap();
            let body = crypto::encrypt_payload_for_recipient(
                &crypto::b64_decode(&selected.kem_pubkey).unwrap(),
                &postcard::to_allocvec(&command).unwrap(),
            )
            .unwrap();
            let response_body = client
                .post(format!("{agent_url}/api/v1/command"))
                .body(body)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .bytes()
                .await
                .unwrap();
            let response: WorkloadCommandResponse = postcard::from_bytes(
                &crypto::decrypt_payload_from_recipient_blob(&response_body, &response_private)
                    .unwrap(),
            )
            .unwrap();
            response.verify().unwrap();
            assert!(response.ok);
        }

        agent.abort();
        scheduler.abort();
    }
}
