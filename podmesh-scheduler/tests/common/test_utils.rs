use podmesh_scheduler::{
    Cli, sidecar::DEFAULT_SIDECAR_BOOTSTRAP_MULTIADDR,
    sidecar::DEFAULT_SIDECAR_IMAGE, start_machine,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::sleep;

static CLEANUP_HOOK_INIT: Once = Once::new();

/// Maximum time to wait for a port to become available.
const PORT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Interval between port availability checks.
const PORT_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Wait for a TCP port to become available for binding.
/// Returns Ok(()) if the port is available, Err if timeout is reached.
#[allow(dead_code)]
pub async fn wait_for_port_available(port: u16, host: &str) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid address: {}", e))?;

    let deadline = tokio::time::Instant::now() + PORT_WAIT_TIMEOUT;

    while tokio::time::Instant::now() < deadline {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                // Port is available, drop the listener to release it
                drop(listener);
                return Ok(());
            }
            Err(_) => {
                // Port still in use, wait and retry
                sleep(PORT_CHECK_INTERVAL).await;
            }
        }
    }

    Err(anyhow::anyhow!(
        "Timeout waiting for port {} to become available",
        port
    ))
}

/// Wait for multiple ports to become available.
#[allow(dead_code)]
pub async fn wait_for_ports_available(ports: &[u16], host: &str) -> anyhow::Result<()> {
    for port in ports {
        wait_for_port_available(*port, host).await?;
    }
    Ok(())
}

/// Clean up potentially corrupted key files from ~/.podmesh before tests
#[allow(dead_code)]
pub fn cleanup_key_files() {
    if let Some(home) = dirs::home_dir() {
        let podmesh_dir = home.join(".podmesh");
        if podmesh_dir.exists() {
            eprintln!("Cleaning up key files from {:?}", podmesh_dir);
            let _ = std::fs::remove_dir_all(&podmesh_dir);
        }
    }
}

/// Set env var for tests while containing the unsafe block required by Rust 2024.
#[allow(dead_code)]
pub fn set_env_var(key: &str, value: &str) {
    unsafe {
        std::env::set_var(key, value);
    }
}

/// Remove env var for tests while containing the unsafe block required by Rust 2024.
#[allow(dead_code)]
pub fn remove_env_var(key: &str) {
    unsafe {
        std::env::remove_var(key);
    }
}

#[allow(dead_code)]
pub struct NodeGuard {
    pub handles: Vec<JoinHandle<()>>,
    pub processes: Vec<Child>,
    cleaned_up: bool,
}

impl NodeGuard {
    #[allow(dead_code)]
    pub async fn cleanup(&mut self) {
        if self.cleaned_up {
            return;
        }

        for h in self.handles.drain(..) {
            let _ = h.abort();
        }

        for mut process in self.processes.drain(..) {
            let _ = process.kill().await;
        }

        self.cleaned_up = true;
    }
}

impl Drop for NodeGuard {
    fn drop(&mut self) {
        if self.cleaned_up {
            return;
        }

        eprintln!("NodeGuard::drop() - Running emergency cleanup");

        for handle in self.handles.drain(..) {
            let _ = handle.abort();
        }

        for mut process in self.processes.drain(..) {
            let _ = process.start_kill();
        }

        global_cleanup();
    }
}

fn workspace_root() -> PathBuf {
    let machine_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    machine_dir
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[allow(dead_code)]
pub fn make_test_cli(
    rest_api_port: u16,
    disable_rest: bool,
    disable_machine: bool,
    api_socket: Option<String>,
    bootstrap_peers: Vec<String>,
    libp2p_quic_port: u16,
    disable_scheduling: bool,
) -> Cli {
    Cli {
        ephemeral: true,
        rest_api_host: "127.0.0.1".to_string(),
        rest_api_port,
        disable_rest_api: disable_rest,
        disable_machine_api: disable_machine,
        node_name: None,
        api_socket,
        key_dir: String::from("/tmp/.podmesh_test_unused"),
        bootstrap_peer: bootstrap_peers,
        libp2p_quic_port,
        libp2p_host: "0.0.0.0".to_string(),
        disable_scheduling,
        mode: if disable_scheduling {
            podmesh_scheduler::NodeMode::Custodian
        } else {
            podmesh_scheduler::NodeMode::Both
        },
        mock_only_runtime: true,
        podman_socket: std::env::var("PODMAN_HOST").ok().and_then(|value| {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }),
        signing_ephemeral: true,
        kem_ephemeral: true,
        ephemeral_keys: true,
        sidecar_bootstrap_peer: DEFAULT_SIDECAR_BOOTSTRAP_MULTIADDR.to_string(),
        sidecar_image: DEFAULT_SIDECAR_IMAGE.to_string(),
    }
}

/// Start a list of nodes in separate processes.
#[allow(dead_code)]
pub async fn start_nodes_as_processes(clis: Vec<Cli>, startup_delay: Duration) -> NodeGuard {
    let mut guard = NodeGuard {
        handles: Vec::new(),
        processes: Vec::new(),
        cleaned_up: false,
    };

    let machine_binary = workspace_root().join("target/debug/machine");

    if !machine_binary.exists() {
        panic!(
            "machine binary not found at {:?}. Run 'cargo build' first.",
            machine_binary
        );
    }

    for cli in clis {
        // Collect ports that need to be available before starting this node
        let mut ports_to_wait: Vec<u16> = Vec::new();
        if !cli.disable_rest_api {
            ports_to_wait.push(cli.rest_api_port);
        }
        // Only wait for libp2p port if it's not 0 (auto-assigned)
        if cli.libp2p_quic_port != 0 {
            ports_to_wait.push(cli.libp2p_quic_port);
        }

        // Wait for required ports to become available
        if let Err(e) = wait_for_ports_available(&ports_to_wait, &cli.rest_api_host).await {
            panic!(
                "Ports {:?} not available for node (rest_api_port={}): {}",
                ports_to_wait, cli.rest_api_port, e
            );
        }

        let mut cmd = Command::new(&machine_binary);
        cmd.arg("--ephemeral")
            .arg("--rest-api-host")
            .arg(&cli.rest_api_host)
            .arg("--rest-api-port")
            .arg(&cli.rest_api_port.to_string())
            .arg("--libp2p-quic-port")
            .arg(&cli.libp2p_quic_port.to_string())
            .arg("--libp2p-host")
            .arg(&cli.libp2p_host);

        if cli.disable_rest_api {
            cmd.arg("--disable-rest-api");
        }
        if cli.disable_machine_api {
            cmd.arg("--disable-machine-api");
        }
        if cli.disable_scheduling {
            cmd.arg("--disable-scheduling");
        }

        if cli.mock_only_runtime {
            cmd.arg("--mock-only-runtime");
        }

        if cli.signing_ephemeral {
            cmd.arg("--signing-ephemeral");
        }

        if cli.kem_ephemeral {
            cmd.arg("--kem-ephemeral");
        }

        if cli.ephemeral_keys {
            cmd.arg("--ephemeral-keys");
        }

        if let Some(socket) = &cli.podman_socket {
            cmd.arg("--podman-socket").arg(socket);
        }

        for bootstrap in &cli.bootstrap_peer {
            cmd.arg("--bootstrap-peer").arg(bootstrap);
        }

        cmd.env("RUST_LOG", "info,libp2p=warn,quinn=warn");

        match cmd.spawn() {
            Ok(child) => {
                guard.processes.push(child);
            }
            Err(e) => panic!("failed to start machine process: {:?}", e),
        }

        sleep(startup_delay).await;
    }

    guard
}

/// Start a list of nodes given their CLIs.
#[allow(dead_code)]
pub async fn start_nodes(clis: Vec<Cli>, startup_delay: Duration) -> NodeGuard {
    let mut guard = NodeGuard {
        handles: Vec::new(),
        processes: Vec::new(),
        cleaned_up: false,
    };
    for cli in clis {
        // Collect ports that need to be available before starting this node
        let mut ports_to_wait: Vec<u16> = Vec::new();
        if !cli.disable_rest_api {
            ports_to_wait.push(cli.rest_api_port);
        }
        // Only wait for libp2p port if it's not 0 (auto-assigned)
        if cli.libp2p_quic_port != 0 {
            ports_to_wait.push(cli.libp2p_quic_port);
        }

        // Wait for required ports to become available
        if let Err(e) = wait_for_ports_available(&ports_to_wait, &cli.rest_api_host).await {
            panic!(
                "Ports {:?} not available for node (rest_api_port={}): {}",
                ports_to_wait, cli.rest_api_port, e
            );
        }

        match start_machine(cli).await {
            Ok(mut handles) => {
                guard.handles.append(&mut handles);
            }
            Err(e) => panic!("failed to start node: {:?}", e),
        }
        sleep(startup_delay).await;
    }
    guard
}

/// Setup global panic hook for test cleanup.
#[allow(dead_code)]
pub fn setup_cleanup_hook() {
    CLEANUP_HOOK_INIT.call_once(|| {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            eprintln!("Test panic detected, running global cleanup...");
            global_cleanup();
            default_hook(info);
        }));
    });
    // Reset process-global state that must not leak between tests.
    crypto::nonce_helper::reset_nonce_store_for_test();
}

/// Global cleanup function that runs system commands to clean up test artifacts
pub fn global_cleanup() {
    eprintln!("Running global cleanup: pkill + rm commands");

    let pkill_result = std::process::Command::new("pkill")
        .args(["-f", "target/debug/machine"])
        .output();

    if let Ok(output) = pkill_result {
        if !output.stdout.is_empty() {
            eprintln!("pkill stdout: {}", String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            eprintln!("pkill stderr: {}", String::from_utf8_lossy(&output.stderr));
        }
    }

    // Clean up potentially corrupted key files from ~/.podmesh
    if let Some(home) = dirs::home_dir() {
        let podmesh_dir = home.join(".podmesh");
        if podmesh_dir.exists() {
            eprintln!("Cleaning up key files from {:?}", podmesh_dir);
            let _ = std::fs::remove_dir_all(&podmesh_dir);
        }
    }

    eprintln!("Global cleanup completed");
}
