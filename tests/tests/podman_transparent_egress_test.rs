//! Integration test for transparent egress proxy through Podman
//!
//! This test verifies that the sidecar's nftables-based transparent egress proxy
//! properly intercepts outbound traffic and tunnels it through the proxy node.
//!
//! Test flow:
//! 1. Deploy podmesh stack (scheduler + proxy)
//! 2. Provision the proxy with the workload tenant's signed certificate
//! 3. Deploy test workload with curl client + sidecar (enable_egress=true, CAP_NET_ADMIN)
//! 4. Execute curl to the scheduler health endpoint from within the workload pod
//! 5. Verify interception, tunnel establishment, proxy handling, and the HTTP 200 response

#![cfg(feature = "podman-tests")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use podctl::{apply_file_with_proxy_multiaddrs, delete_file};
use podmesh_agent::sidecar::workload_runtime_name;
use podmesh_integration_tests::support::{
    init_ephemeral_keys, init_tracing, prepare_podman_proxy_topology, provision_proxy_cert,
};
use reqwest::Client;
use serde_json::Value;
use serial_test::serial;
use tokio::{process::Command as TokioCommand, time::sleep};

const MACHINE_API_URL: &str = "http://127.0.0.1:3000";
const ROOTLESS_MANIFEST_PATH: &str = "deploy/podmesh_rootless.yml";
const ROOTFUL_MANIFEST_PATH: &str = "deploy/podmesh_rootful.yml";
const EGRESS_TEST_MANIFEST_PATH: &str = "tests/sample_manifests/transparent_egress_test.yml";
const EGRESS_TEST_URL: &str = "http://scheduler:3000/health";
const PODMESH_PROXY_API_PORTS: [u16; 3] = [3001, 3002, 3003];
const PODMESH_NETWORK: &str = "podmesh";
const ROOTLESS_PODMAN_SOCKET: &str = "/run/user/1000/podman/podman.sock";
const ROOTFUL_PODMAN_SOCKET: &str = "/run/podman/podman.sock";
const REQUIRED_IMAGES: [&str; 4] = [
    "localhost/podmesh/scheduler:latest",
    "localhost/podmesh/agent:latest",
    "localhost/podmesh/proxy:latest",
    "localhost/podmesh/sidecar:latest",
];

// Log patterns to verify transparent proxy is working
const SIDECAR_EGRESS_LISTENING_PATTERN: &str = "Egress proxy listening";
const SIDECAR_EGRESS_CONNECTION_PATTERN: &str = "Egress connection from";
const SIDECAR_EGRESS_TUNNEL_ESTABLISHED_PATTERN: &str = "egress tunnel established";
const SIDECAR_REGISTRATION_PATTERN: &str = "sidecar registration acknowledged";
const PROXY_EGRESS_TUNNEL_PATTERN: &str = "egress tunnel";

/// Integration test that validates transparent egress proxy through podman.
///
/// This test verifies that:
/// 1. The sidecar sets up nftables rules for transparent egress (requires CAP_NET_ADMIN)
/// 2. Outbound HTTP requests from the app container are intercepted
/// 3. Traffic flows through the sidecar's transparent proxy to the proxy node
/// 4. The proxy node handles the egress tunnel and forwards to external destination
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn transparent_egress_routes_through_sidecar_and_proxy() -> Result<()> {
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
    let egress_test_manifest = workspace.join(EGRESS_TEST_MANIFEST_PATH);

    // Start the podmesh stack
    let mut stack_guard = PodmanKubeGuard::launch(&stack_manifest).await?;
    let mut workload_guard = WorkloadGuard::default();

    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to build HTTP client")?;

    // Wait for machine to be healthy and peers to register
    wait_for_machine_health(&client, Duration::from_secs(120)).await?;
    wait_for_agent_registration(&client, Duration::from_secs(120)).await?;
    let proxy_peer_ids = wait_for_proxy_peer_ids(&client, Duration::from_secs(120)).await?;
    let (owner_public, owner_private) = crypto::ensure_keypair_on_disk()?;
    for port in PODMESH_PROXY_API_PORTS {
        provision_proxy_cert(
            port,
            &owner_public,
            &owner_private,
            Duration::from_secs(120),
        )
        .await
        .with_context(|| format!("failed to provision tenant proxy certificate on {port}"))?;
    }

    // Deploy the egress test workload
    let manifest_id = apply_file_with_proxy_multiaddrs(
        egress_test_manifest.clone(),
        Some(MACHINE_API_URL),
        proxy_peer_ids
            .into_iter()
            .enumerate()
            .map(|(index, peer_id)| {
                format!("/dns4/proxy/udp/{}/quic-v1/p2p/{peer_id}", 4002 + index)
            })
            .collect(),
    )
    .await
    .context("podctl apply failed for egress test manifest")?;
    log::info!("podctl applied egress test manifest {manifest_id}");
    workload_guard.set(manifest_id.clone());

    // Wait for workload containers to be running
    wait_for_egress_test_containers(&manifest_id, Duration::from_secs(180)).await?;
    wait_for_sidecar_registration(&manifest_id, Duration::from_secs(120)).await?;

    let curl_output = execute_egress_curl(&manifest_id).await?;
    log::info!("egress curl succeeded: {} bytes", curl_output.len());

    // Wait a moment for logs to be written
    sleep(Duration::from_secs(2)).await;

    // Verify sidecar logs show transparent proxy activity (nftables redirect working)
    // This validates our rustables/netlink nftables implementation
    verify_sidecar_egress_logs(&manifest_id).await?;

    verify_proxy_egress_logs().await?;

    log::info!(
        "transparent egress proxy test passed - traffic successfully routed through sidecar and proxy"
    );

    // Cleanup
    delete_file(egress_test_manifest.clone(), true, Some(MACHINE_API_URL))
        .await
        .context("podctl delete failed")?;
    wait_for_workload_teardown(&manifest_id, Duration::from_secs(90)).await?;
    workload_guard.disarm();

    stack_guard.shutdown().await?;

    Ok(())
}

// ============================================================================
// Helper functions
// ============================================================================

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

async fn wait_for_proxy_peer_ids(client: &Client, timeout: Duration) -> Result<Vec<String>> {
    let deadline = Instant::now() + timeout;
    let mut peer_ids = vec![None; PODMESH_PROXY_API_PORTS.len()];
    while Instant::now() < deadline {
        for (index, port) in PODMESH_PROXY_API_PORTS.iter().enumerate() {
            if peer_ids[index].is_some() {
                continue;
            }
            let url = format!("http://127.0.0.1:{port}/api/v1/peer_id");
            if let Ok(response) = client.get(&url).send().await
                && response.status().is_success()
            {
                let value: Value = response.json().await?;
                peer_ids[index] = value
                    .get("peer_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
        if peer_ids.iter().all(Option::is_some) {
            return Ok(peer_ids.into_iter().flatten().collect());
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err(anyhow!("not all regional proxy peer IDs became available"))
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
    let output = run_podman_command(&["images", "--format", "json"]).await?;
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
        let _ = run_podman_command(&["kube", "down", &manifest_arg]).await;
        prepare_podman_proxy_topology().await?;
        run_podman_command(&["kube", "play", "--network", PODMESH_NETWORK, &manifest_arg])
            .await
            .context("failed to start podmesh stack with podman kube play")?;
        log::info!(
            "started podmesh stack via podman kube play using {}",
            manifest_path.display()
        );
        Ok(Self {
            manifest_path: manifest_path.to_path_buf(),
        })
    }

    async fn shutdown(&mut self) -> Result<()> {
        let manifest_arg = self.manifest_path.to_string_lossy().to_string();
        run_podman_command(&["kube", "down", &manifest_arg])
            .await
            .context("failed to stop podmesh stack with podman kube down")?;
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

async fn wait_for_egress_test_containers(workload_name: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let runtime_name = workload_runtime_name(workload_name);
    let targets: HashSet<&str> = ["curl-client", "sidecar"].iter().copied().collect();
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
                        "egress test workload containers are running for workload {}",
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
            ["curl-client", "sidecar"],
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

async fn wait_for_sidecar_registration(workload_name: &str, timeout: Duration) -> Result<()> {
    let sidecar_container = find_container_by_pattern(workload_name, "sidecar").await?;
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let logs = get_container_logs(&sidecar_container).await?;
        if logs.contains(SIDECAR_REGISTRATION_PATTERN) {
            log::info!("sidecar registered with an authenticated tenant proxy");
            return Ok(());
        }
        sleep(Duration::from_millis(500)).await;
    }

    Err(anyhow!(
        "sidecar did not register with an authenticated tenant proxy"
    ))
}

/// Execute an HTTP request from the workload pod to another pod in the mesh.
/// The request must be intercepted and tunneled through the authenticated proxy.
async fn execute_egress_curl(workload_name: &str) -> Result<String> {
    let curl_container = find_container_by_pattern(workload_name, "curl-client").await?;

    log::info!(
        "executing curl to {} from container {}",
        EGRESS_TEST_URL,
        curl_container,
    );

    // Retry curl with increasing delays to allow proxy peer discovery
    const MAX_RETRIES: u32 = 5;
    const RETRY_DELAY_SECS: u64 = 5;

    let mut last_error = None;
    for attempt in 1..=MAX_RETRIES {
        log::info!("curl attempt {}/{}", attempt, MAX_RETRIES);

        let result = run_podman_command(&[
            "exec",
            &curl_container,
            "curl",
            "-s",
            "-S", // Show errors
            "-m",
            "15", // 15 second timeout per attempt
            "-w",
            "\nHTTP_STATUS:%{http_code}\n",
            EGRESS_TEST_URL,
        ])
        .await;

        match result {
            Ok(output) => {
                log::info!("curl output: {}", output);

                if output.contains("HTTP_STATUS:200") {
                    log::info!("received successful HTTP response through egress tunnel");
                    return Ok(output);
                } else {
                    last_error = Some(anyhow!(
                        "egress destination did not return HTTP 200: {}",
                        output
                    ));
                }
            }
            Err(e) => {
                log::warn!("curl attempt {} failed: {}", attempt, e);
                last_error = Some(e);
            }
        }

        if attempt < MAX_RETRIES {
            log::info!("waiting {}s before retry...", RETRY_DELAY_SECS);
            sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("curl failed after {} attempts", MAX_RETRIES)))
        .context("egress request failed")
}

/// Verify sidecar logs contain evidence of transparent proxy activity
async fn verify_sidecar_egress_logs(workload_name: &str) -> Result<()> {
    let sidecar_container = find_container_by_pattern(workload_name, "sidecar").await?;
    let logs = get_container_logs(&sidecar_container).await?;

    log::debug!("sidecar logs:\n{}", logs);

    let mut found_patterns = Vec::new();
    let mut missing_patterns = Vec::new();

    // Check for egress proxy listener startup
    if logs.contains(SIDECAR_EGRESS_LISTENING_PATTERN) {
        found_patterns.push(SIDECAR_EGRESS_LISTENING_PATTERN);
    } else {
        missing_patterns.push(SIDECAR_EGRESS_LISTENING_PATTERN);
    }

    // Check for intercepted egress connection (from nftables redirect)
    if logs.contains(SIDECAR_EGRESS_CONNECTION_PATTERN) {
        found_patterns.push(SIDECAR_EGRESS_CONNECTION_PATTERN);
    } else {
        missing_patterns.push(SIDECAR_EGRESS_CONNECTION_PATTERN);
    }

    // Check for successful tunnel establishment
    if logs.contains(SIDECAR_EGRESS_TUNNEL_ESTABLISHED_PATTERN) {
        found_patterns.push(SIDECAR_EGRESS_TUNNEL_ESTABLISHED_PATTERN);
    } else {
        missing_patterns.push(SIDECAR_EGRESS_TUNNEL_ESTABLISHED_PATTERN);
    }

    log::info!(
        "sidecar log patterns found: {:?}, missing: {:?}",
        found_patterns,
        missing_patterns
    );

    anyhow::ensure!(
        missing_patterns.is_empty(),
        "sidecar lacks required transparent egress evidence; found={found_patterns:?} missing={missing_patterns:?}"
    );

    Ok(())
}

/// Verify proxy logs show egress tunnel handling
async fn verify_proxy_egress_logs() -> Result<()> {
    let containers = capture_podman_containers().await?;
    let proxy_containers: Vec<String> = containers
        .iter()
        .flat_map(|container| container.names.iter())
        .filter(|name| name.starts_with("proxy-proxy"))
        .cloned()
        .collect();
    anyhow::ensure!(!proxy_containers.is_empty(), "no proxy containers found");

    for container_name in proxy_containers {
        let logs = get_container_logs(&container_name).await?;
        log::debug!("proxy logs:\n{}", logs);

        if logs.contains(PROXY_EGRESS_TUNNEL_PATTERN) {
            log::info!("proxy logs show egress tunnel activity");
            return Ok(());
        }
    }

    Err(anyhow!("proxy logs contain no egress tunnel activity"))
}

/// Get logs from a container
async fn get_container_logs(container_name: &str) -> Result<String> {
    // Container logs may go to stdout or stderr depending on how the app writes them.
    // We need to capture both streams to get complete logs.
    let mut cmd = TokioCommand::new("podman");
    cmd.args(["logs", container_name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = cmd.output().await.context(format!(
        "failed to get logs for container {}",
        container_name
    ))?;

    // Combine stdout and stderr - container output may be in either stream
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Return combined output (most container output goes to stderr)
    let combined = format!("{}{}", stdout, stderr);
    Ok(combined)
}

/// Find a container matching the manifest ID and name pattern
async fn find_container_by_pattern(workload_name: &str, pattern: &str) -> Result<String> {
    let runtime_name = workload_runtime_name(workload_name);
    let containers = capture_podman_containers().await?;

    for container in &containers {
        if container.matches(&runtime_name, pattern)
            && let Some(name) = container.names.first()
        {
            return Ok(name.clone());
        }
    }

    Err(anyhow!(
        "could not find container matching workload={} pattern={}",
        workload_name,
        pattern
    ))
}

async fn capture_podman_containers() -> Result<Vec<PodmanContainer>> {
    let output = run_podman_command(&["ps", "-a", "--format", "json"]).await?;
    parse_podman_ps(&output)
}

async fn run_podman_command(args: &[&str]) -> Result<String> {
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
    match run_podman_command(&["network", "exists", network]).await {
        Ok(_) => Ok(()),
        Err(_) => run_podman_command(&["network", "create", network])
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
