use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    routing::{get, post},
};
use podctl::ClientOptions;
use podmesh_integration_tests::{
    mesh::{MESH_TIMEOUT, TestMesh, test_proxy_endpoint},
    support::{self, ClientKeyDir},
};
use protocol::{AGENT_PROTOCOL_VERSION, CAPACITY_PROTOCOL_VERSION};
use serial_test::serial;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);
const WORKLOAD_RELAY_TOKEN: &str = "podmesh-test-relay-token-000000000001";
const FAKE_AGENT_COUNT: usize = 4;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn manifest(name: &str, cpu_milli: u32) -> Result<(tempfile::TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join(format!("{name}.yaml"));
    std::fs::write(
        &path,
        format!(
            "apiVersion: apps/v1\n\
             kind: Deployment\n\
             metadata:\n  \
               name: {name}\n\
             spec:\n  \
               replicas: 1\n  \
               template:\n    \
                 spec:\n      \
                   containers:\n        \
                     - name: app\n          \
                       image: nginx:alpine\n          \
                       resources:\n            \
                         requests:\n              \
                           cpu: {cpu_milli}m\n              \
                           memory: 64Mi\n"
        ),
    )?;
    Ok((dir, path))
}

async fn apply(path: &Path, options: &ClientOptions) -> Result<String> {
    podctl::apply_file_with_proxy_endpoints(
        path.to_path_buf(),
        options,
        vec![crypto::b64_encode(
            &test_proxy_endpoint()?.to_bytes(now_secs())?,
        )],
        WORKLOAD_RELAY_TOKEN.to_string(),
        Vec::new(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn measured_resources_skip_the_nearly_full_agent() -> Result<()> {
    support::init_tracing();
    let mesh = timeout(MESH_TIMEOUT, TestMesh::start(2))
        .await
        .context("mesh start timed out")??;
    let key_dir = ClientKeyDir::with_trusted_agents(&mesh.trusted_agent_keys())?;
    key_dir.activate();
    let options = ClientOptions::with_api_base(Some(&mesh.api_base));
    let (_large_dir, large_path) = manifest("large-placement", 7_000)?;
    let large_id = timeout(TEST_TIMEOUT, apply(&large_path, &options))
        .await
        .context("large apply timed out")??;
    let large = podctl::catalog::load(key_dir.path(), &large_id)?;

    let (_next_dir, next_path) = manifest("must-fit-elsewhere", 2_000)?;
    let next_id = timeout(TEST_TIMEOUT, apply(&next_path, &options))
        .await
        .context("second apply timed out")??;
    let next = podctl::catalog::load(key_dir.path(), &next_id)?;
    ensure!(
        large.replicas[0].agent_endpoint_id != next.replicas[0].agent_endpoint_id,
        "the scheduler selected the nearly full agent for a workload it could not fit"
    );
    Ok(())
}

struct FakeAgent {
    endpoint_id: String,
    endpoint: protocol::EndpointRecord,
    signing_public: Vec<u8>,
    signing_private: Vec<u8>,
    kem_private: Vec<u8>,
    kem_public: Vec<u8>,
}

#[derive(Clone)]
struct RaceState {
    agents: Arc<Vec<FakeAgent>>,
    selections: Arc<AtomicUsize>,
}

impl RaceState {
    fn new() -> Result<Self> {
        let mut agents = Vec::with_capacity(FAKE_AGENT_COUNT);
        for index in 0..FAKE_AGENT_COUNT {
            let (signing_public, signing_private) = crypto::generate_signing_keypair();
            let (kem_public, kem_private) = crypto::generate_kem_keypair();
            let endpoint_id = iroh::SecretKey::generate().public();
            let endpoint = protocol::EndpointRecord {
                version: protocol::ENDPOINT_RECORD_VERSION,
                endpoint_id: endpoint_id.as_bytes().to_vec(),
                relay_url: None,
                direct_addresses: vec![format!("127.0.0.1:{}", 41_000 + index)],
                signing_pubkey: String::new(),
                issued_at_secs: now_secs(),
                expires_at_secs: now_secs() + 300,
                signature: String::new(),
            }
            .sign(&signing_public, &signing_private, now_secs())?;
            agents.push(FakeAgent {
                endpoint_id: hex::encode(endpoint_id.as_bytes()),
                endpoint,
                signing_public,
                signing_private,
                kem_private,
                kem_public,
            });
        }
        Ok(Self {
            agents: Arc::new(agents),
            selections: Arc::new(AtomicUsize::new(0)),
        })
    }
}

async fn list_empty() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "answered": [],
        "unreachable_agents": [],
        "unreachable_schedulers": []
    }))
}

async fn select(
    State(state): State<RaceState>,
) -> Result<axum::Json<protocol::CapacityOffer>, StatusCode> {
    let index = state.selections.fetch_add(1, Ordering::SeqCst);
    let agent = state
        .agents
        .get(index)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let now = now_secs();
    let offer = protocol::CapacityOffer {
        version: CAPACITY_PROTOCOL_VERSION,
        query_id: format!("capacity-race-{index}"),
        agent_endpoint: agent.endpoint.clone(),
        kem_pubkey: crypto::b64_encode(&agent.kem_public),
        available_cpu_milli: 8_000,
        available_memory_bytes: 8 * 1024 * 1024 * 1024,
        available_storage_bytes: 64 * 1024 * 1024 * 1024,
        capabilities: vec!["multi-workload".into()],
        issued_at_secs: now,
        expires_at_secs: now + 10,
        signing_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(&agent.signing_public, &agent.signing_private, now)
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(axum::Json(offer))
}

async fn reject_admission(
    State(state): State<RaceState>,
    AxumPath(agent_id): AxumPath<String>,
    body: Bytes,
) -> Result<Vec<u8>, StatusCode> {
    let agent = state
        .agents
        .iter()
        .find(|agent| agent.endpoint_id == agent_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let plaintext = crypto::decrypt_payload_from_recipient_blob(&body, &agent.kem_private)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let request: protocol::AdmissionRequest =
        postcard::from_bytes(&plaintext).map_err(|_| StatusCode::BAD_REQUEST)?;
    request
        .verify(now_secs())
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let reservation = protocol::Reservation {
        version: AGENT_PROTOCOL_VERSION,
        reservation_id: format!("refused-{}", request.request_id),
        request_id: request.request_id,
        namespace_id: request.namespace_id,
        workload_id: request.workload_id,
        agent_node_id: String::new(),
        cpu_milli: request.cpu_milli,
        memory_bytes: request.memory_bytes,
        storage_bytes: request.storage_bytes,
        accepted: false,
        reason: "insufficient capacity".into(),
        expires_at_secs: now_secs() + 30,
        signature: String::new(),
    }
    .sign(&agent.signing_public, &agent.signing_private)
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    crypto::encrypt_payload_for_recipient(
        &crypto::b64_decode(&request.response_kem_pubkey).map_err(|_| StatusCode::BAD_REQUEST)?,
        &postcard::to_allocvec(&reservation).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn admission_capacity_races_stop_at_the_retry_bound() -> Result<()> {
    support::init_tracing();
    let state = RaceState::new()?;
    let trusted = state
        .agents
        .iter()
        .map(|agent| crypto::b64_encode(&agent.signing_public))
        .collect::<Vec<_>>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let api_base = format!("http://{}", listener.local_addr()?);
    let router = Router::new()
        .route("/api/v1/workloads/list", post(list_empty))
        .route("/api/v1/agents/select", get(select))
        .route(
            "/api/v1/agents/{agent_id}/admission",
            post(reject_admission),
        )
        .with_state(state.clone());
    let server = tokio::spawn(axum::serve(listener, router).into_future());

    let key_dir = ClientKeyDir::with_trusted_agents(&trusted)?;
    key_dir.activate();
    let options = ClientOptions::with_api_base(Some(&api_base));
    let (_dir, path) = manifest("capacity-race", 100)?;
    let error = timeout(TEST_TIMEOUT, apply(&path, &options))
        .await
        .context("capacity-race apply timed out")?
        .expect_err("capacity refusals must eventually stop retrying");
    ensure!(
        format!("{error:#}").contains("exhausted 3 admission capacity retries"),
        "unexpected retry error: {error:#}"
    );
    ensure!(
        state.selections.load(Ordering::SeqCst) == FAKE_AGENT_COUNT,
        "podctl did not stop at the configured retry bound"
    );
    server.abort();
    let _ = server.await;
    Ok(())
}
