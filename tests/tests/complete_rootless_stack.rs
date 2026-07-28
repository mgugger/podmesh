#![cfg(feature = "podman-tests")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use podctl::{apply_file_with_proxy_urls, delete_file};
use podmesh_agent::sidecar::workload_runtime_name;
use podmesh_integration_tests::support::{
    init_ephemeral_keys, init_tracing, reset_podman_stack_state,
};
use protocol::MESH_DOMAIN_SUFFIX;
use reqwest::Client;
use serde_json::Value;
use serial_test::serial;
use tokio::{process::Command as TokioCommand, time::sleep};

const MACHINE_API_URL: &str = "http://127.0.0.1:3000";
const ROOTLESS_MANIFEST_PATH: &str = "deploy/podmesh_rootless.yml";
const ROOTFUL_MANIFEST_PATH: &str = "deploy/podmesh_rootful.yml";
const SAMPLE_MANIFEST_PATH: &str = "tests/sample_manifests/demo_deployment_without_sidecar.yml";
const PODMESH_PROXY_URL: &str = "http://127.0.0.1:8080/";
/// REST APIs of the three proxies the deploy manifests start. `podctl`
/// bootstraps its proxy endpoints, relay token, and relay CA certificates from
/// these and mints an owner-signed grant for each.
const PODMESH_PROXY_API_URLS: &str =
    "http://127.0.0.1:3010,http://127.0.0.1:3011,http://127.0.0.1:3012";
const EXPECTED_BODY_SUBSTRING: &str = "Welcome to Podmesh";
const EXPECTED_CONTAINERS: [&str; 2] = ["my-nginx", "sidecar"];
/// Workload name declared by the sample manifest, used to derive the same
/// per-replica workload id the agent names its pod after.
const SAMPLE_WORKLOAD_NAME: &str = "my-nginx";
/// The sample manifest declares a single replica.
const SAMPLE_REPLICA_INDEX: u32 = 0;
const PODMESH_NETWORK: &str = "podmesh";
/// `GET /api/v1/agents/select` gossips a capacity query and waits for offers,
/// which takes several seconds, so probes must outlast a full solicitation.
const HTTP_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const ROOTLESS_PODMAN_SOCKET: &str = "/run/user/1000/podman/podman.sock";
const ROOTFUL_PODMAN_SOCKET: &str = "/run/podman/podman.sock";
const REQUIRED_IMAGES: [&str; 4] = [
    "localhost/podmesh/scheduler:latest",
    "localhost/podmesh/agent:latest",
    "localhost/podmesh/proxy:latest",
    "localhost/podmesh/sidecar:latest",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn complete_rootless_stack_serves_ingress() -> Result<()> {
    init_tracing();
    init_ephemeral_keys();

    anyhow::ensure!(
        is_podman_available().await,
        "podman-tests requires the podman CLI"
    );

    // Determine which podman socket and manifest to use
    let (socket_path, manifest_path, mode) = if is_socket_available(ROOTLESS_PODMAN_SOCKET) {
        (ROOTLESS_PODMAN_SOCKET, ROOTLESS_MANIFEST_PATH, "rootless")
    } else if is_socket_available(ROOTFUL_PODMAN_SOCKET) {
        (ROOTFUL_PODMAN_SOCKET, ROOTFUL_MANIFEST_PATH, "rootful")
    } else {
        anyhow::bail!(
            "podman-tests requires a Podman socket; rootless: {} (start with `systemctl --user start podman.socket`), rootful: {}",
            ROOTLESS_PODMAN_SOCKET,
            ROOTFUL_PODMAN_SOCKET
        );
    };
    log::info!("using {mode} podman socket: {socket_path}");

    verify_required_images().await?;

    let workspace = workspace_root();
    let stack_manifest = workspace.join(manifest_path);
    let sample_manifest = workspace.join(SAMPLE_MANIFEST_PATH);

    let mut stack_guard = PodmanKubeGuard::launch(&stack_manifest).await?;
    let mut workload_guard = WorkloadGuard::default();

    let client = Client::builder()
        .timeout(HTTP_PROBE_TIMEOUT)
        .build()
        .context("failed to build HTTP client")?;

    wait_for_machine_health(&client, Duration::from_secs(120)).await?;
    wait_for_agent_registration(&client, Duration::from_secs(180)).await?;

    let manifest_id = apply_file_with_proxy_urls(
        sample_manifest.clone(),
        Some(MACHINE_API_URL),
        PODMESH_PROXY_API_URLS.to_string(),
    )
    .await
    .context("podctl apply failed")?;
    log::info!("podctl applied manifest {manifest_id}");
    // `podctl` returns the deployment id, while the agent names the pod after
    // the per-replica workload id. Derive the latter to find the containers.
    let (owner_public, _owner_private) =
        crypto::ensure_keypair_on_disk().context("load namespace signing key")?;
    let workload_id =
        protocol::workload_id(&owner_public, SAMPLE_WORKLOAD_NAME, SAMPLE_REPLICA_INDEX);
    workload_guard.set(workload_id.clone());

    wait_for_workload_containers(&workload_id, Duration::from_secs(180)).await?;
    wait_for_podmesh_proxy_response(&client, Duration::from_secs(120)).await?;

    // Give workload state time to settle before attempting delete.
    sleep(Duration::from_secs(10)).await;

    delete_file(sample_manifest.clone(), true, Some(MACHINE_API_URL))
        .await
        .context("podctl delete failed")?;
    wait_for_workload_teardown(&workload_id, Duration::from_secs(90)).await?;
    workload_guard.disarm();

    stack_guard.shutdown().await?;

    Ok(())
}

async fn is_podman_available() -> bool {
    match TokioCommand::new("podman")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

fn is_socket_available(socket_path: &str) -> bool {
    let path = Path::new(socket_path);
    match std::fs::metadata(path) {
        Ok(metadata) => is_unix_socket(&metadata),
        Err(_) => false,
    }
}

fn is_unix_socket(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        metadata.file_type().is_socket()
    }

    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

async fn verify_required_images() -> Result<()> {
    let output = run_podman_command(["images", "--format", "json"]).await?;
    let images: Value = serde_json::from_str(&output).context("invalid podman images json")?;

    let available_images: HashSet<String> = images
        .as_array()
        .ok_or_else(|| anyhow!("podman images output was not an array"))?
        .iter()
        .filter_map(|img| {
            let names = img.get("Names").and_then(|n| n.as_array())?;
            Some(
                names
                    .iter()
                    .filter_map(|name| name.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect();

    let mut missing = Vec::new();
    for required in REQUIRED_IMAGES {
        if !available_images.contains(required) {
            missing.push(required);
        }
    }

    if missing.is_empty() {
        log::info!("verified all required container images are available locally");
        Ok(())
    } else {
        Err(anyhow!(
            "missing required container images: {:?}. build them with ./deploy/build_containers.sh",
            missing
        ))
    }
}

struct PodmanKubeGuard {
    manifest_path: PathBuf,
}

impl PodmanKubeGuard {
    async fn launch(manifest_path: &Path) -> Result<Self> {
        let manifest_arg = manifest_path.to_string_lossy().to_string();
        ensure_podman_network(PODMESH_NETWORK).await?;
        let _ = run_podman_command(["kube", "down", &manifest_arg]).await;
        reset_podman_stack_state().await?;
        run_podman_command(["kube", "play", "--network", PODMESH_NETWORK, &manifest_arg])
            .await
            .context("failed to start rootless stack with podman kube play")?;
        log::info!(
            "started podmesh rootless stack via podman kube play using {}",
            manifest_path.display()
        );
        Ok(Self {
            manifest_path: manifest_path.to_path_buf(),
        })
    }

    async fn shutdown(&mut self) -> Result<()> {
        let manifest_arg = self.manifest_path.to_string_lossy().to_string();
        run_podman_command(["kube", "down", &manifest_arg])
            .await
            .context("failed to stop rootless stack with podman kube down")?;
        Ok(())
    }
}

impl Drop for PodmanKubeGuard {
    fn drop(&mut self) {
        let _ = StdCommand::new("podman")
            .arg("kube")
            .arg("down")
            .arg(&self.manifest_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[derive(Default)]
struct WorkloadGuard {
    workload_name: Option<String>,
}

impl WorkloadGuard {
    fn set(&mut self, workload_name: String) {
        self.workload_name = Some(workload_name);
    }

    fn disarm(&mut self) {
        self.workload_name = None;
    }
}

impl Drop for WorkloadGuard {
    fn drop(&mut self) {
        if let Some(workload_name) = self.workload_name.take() {
            let pod_name = format!("{}-pod", workload_runtime_name(&workload_name));
            let _ = StdCommand::new("podman")
                .arg("pod")
                .arg("rm")
                .arg("-f")
                .arg(&pod_name)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

async fn wait_for_machine_health(client: &Client, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let url = format!("{MACHINE_API_URL}/health");
    let mut last_err: Option<anyhow::Error> = None;

    while Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                last_err = Some(anyhow!("unexpected status {}", response.status()));
            }
            Err(err) => last_err = Some(err.into()),
        }
        sleep(Duration::from_millis(500)).await;
    }

    Err(last_err.unwrap_or_else(|| anyhow!("machine REST API never became healthy")))
}

async fn wait_for_agent_registration(client: &Client, timeout: Duration) -> Result<()> {
    let url = format!("{MACHINE_API_URL}/api/v1/agents/select");
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<anyhow::Error> = None;

    while Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                last_err = Some(anyhow!(
                    "agent selection endpoint status {}",
                    response.status()
                ));
            }
            Err(err) => last_err = Some(err.into()),
        }
        sleep(Duration::from_millis(500)).await;
    }

    Err(last_err.unwrap_or_else(|| anyhow!("scheduler never reported an available agent")))
}

async fn wait_for_workload_containers(workload_name: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let runtime_name = workload_runtime_name(workload_name);
    let targets: HashSet<&str> = EXPECTED_CONTAINERS.iter().copied().collect();
    let mut satisfied: HashSet<&str> = HashSet::new();
    let mut last_err: Option<anyhow::Error> = None;

    while Instant::now() < deadline {
        match capture_podman_containers().await {
            Ok(containers) => {
                satisfied.clear();
                for target in &targets {
                    if containers.iter().any(|c| c.matches(&runtime_name, target)) {
                        satisfied.insert(target);
                    }
                }
                if satisfied.len() == targets.len() {
                    log::info!(
                        "nginx workload containers are running for workload {}",
                        workload_name
                    );
                    return Ok(());
                }
            }
            Err(err) => last_err = Some(err),
        }
        sleep(Duration::from_secs(2)).await;
    }

    Err(last_err.unwrap_or_else(|| {
        anyhow!(
            "timed out waiting for containers {:?} belonging to workload {}",
            EXPECTED_CONTAINERS,
            workload_name
        )
    }))
}

async fn wait_for_workload_teardown(workload_name: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let runtime_name = workload_runtime_name(workload_name);

    while Instant::now() < deadline {
        match capture_podman_containers().await {
            Ok(containers) => {
                let active = containers
                    .iter()
                    .any(|c| c.belongs_to_workload(&runtime_name));
                if !active {
                    return Ok(());
                }
            }
            Err(err) => {
                log::warn!("podman ps failed during teardown wait: {err:?}");
            }
        }
        sleep(Duration::from_secs(2)).await;
    }

    Err(anyhow!(
        "workload containers for {} never terminated after delete",
        workload_name
    ))
}

async fn wait_for_podmesh_proxy_response(client: &Client, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<anyhow::Error> = None;
    let ingress_host_header = format!("demo-nginx.{}", MESH_DOMAIN_SUFFIX);

    while Instant::now() < deadline {
        match client
            .get(PODMESH_PROXY_URL)
            .header("host", &ingress_host_header)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => match response.text().await {
                Ok(body) => {
                    if body.contains(EXPECTED_BODY_SUBSTRING) {
                        log::info!("podmesh-proxy served expected content for ingress host");
                        return Ok(body);
                    } else {
                        last_err = Some(anyhow!("unexpected ingress body: {body}"));
                    }
                }
                Err(err) => last_err = Some(err.into()),
            },
            Ok(response) => {
                last_err = Some(anyhow!("podmesh-proxy status {}", response.status()));
            }
            Err(err) => last_err = Some(err.into()),
        }

        sleep(Duration::from_millis(500)).await;
    }

    Err(last_err.unwrap_or_else(|| anyhow!("podmesh-proxy never returned the expected page")))
}

async fn capture_podman_containers() -> Result<Vec<PodmanContainer>> {
    let output = run_podman_command(["ps", "--format", "json"]).await?;
    parse_podman_ps(&output)
}

async fn run_podman_command<const N: usize>(args: [&str; N]) -> Result<String> {
    let mut cmd = TokioCommand::new("podman");
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = cmd.output().await.context("failed to run podman command")?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "podman {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

async fn ensure_podman_network(network: &str) -> Result<()> {
    match run_podman_command(["network", "exists", network]).await {
        Ok(_) => Ok(()),
        Err(_) => run_podman_command(["network", "create", network])
            .await
            .context(format!("failed to create podman network {network}"))
            .map(|_| ()),
    }
}

fn parse_podman_ps(output: &str) -> Result<Vec<PodmanContainer>> {
    let value: Value = serde_json::from_str(output).context("invalid podman ps json")?;
    let containers = value
        .as_array()
        .ok_or_else(|| anyhow!("podman ps output was not an array"))?
        .iter()
        .map(PodmanContainer::try_from)
        .collect::<Result<Vec<_>>>()?;
    Ok(containers)
}

#[derive(Debug)]
struct PodmanContainer {
    names: Vec<String>,
    state: ContainerState,
}

impl PodmanContainer {
    fn matches(&self, workload_name: &str, token: &str) -> bool {
        self.state == ContainerState::Running
            && self
                .names
                .iter()
                .any(|name| name.contains(workload_name) && name.contains(token))
    }

    fn belongs_to_workload(&self, workload_name: &str) -> bool {
        self.names.iter().any(|name| name.contains(workload_name))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ContainerState {
    Running,
    Other,
}

impl TryFrom<&Value> for PodmanContainer {
    type Error = anyhow::Error;

    fn try_from(value: &Value) -> Result<Self> {
        let names = extract_container_names(value);
        let state = extract_container_state(value);
        Ok(Self { names, state })
    }
}

fn extract_container_names(value: &Value) -> Vec<String> {
    if let Some(array) = value.get("Names").and_then(|n| n.as_array()) {
        let mut names: Vec<String> = array
            .iter()
            .filter_map(|entry| entry.as_str().map(|s| s.to_string()))
            .collect();
        if names.is_empty()
            && let Some(name) = value.get("Names").and_then(|n| n.as_str())
        {
            names.push(name.to_string());
        }
        if names.is_empty()
            && let Some(name) = value.get("Name").and_then(|n| n.as_str())
        {
            names.push(name.to_string());
        }
        names
    } else if let Some(name) = value.get("Names").and_then(|n| n.as_str()) {
        vec![name.to_string()]
    } else if let Some(name) = value.get("Name").and_then(|n| n.as_str()) {
        vec![name.to_string()]
    } else {
        Vec::new()
    }
}

fn extract_container_state(value: &Value) -> ContainerState {
    let mut candidates = Vec::new();

    if let Some(state_value) = value.get("State") {
        if let Some(state_str) = state_value.as_str() {
            candidates.push(state_str.to_string());
        } else if let Some(obj) = state_value.as_object()
            && let Some(status) = obj.get("Status").and_then(|s| s.as_str())
        {
            candidates.push(status.to_string());
        }
    }

    if let Some(status) = value.get("Status").and_then(|s| s.as_str()) {
        candidates.push(status.to_string());
    }

    for state in candidates {
        let normalized = state.to_ascii_lowercase();
        if normalized == "running" || normalized.starts_with("up") {
            return ContainerState::Running;
        }
        if normalized.contains("running") {
            return ContainerState::Running;
        }
    }

    ContainerState::Other
}
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests crate must be inside workspace")
        .to_path_buf()
}
