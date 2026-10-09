use std::collections::HashSet;
use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, Once, OnceLock};

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;

static INIT_LOGGING: Once = Once::new();
/// Single volume the deploy manifests mount into every component.
const PODMESH_STATE_VOLUME: &str = "podmesh-state";

pub fn init_tracing() {
    INIT_LOGGING.call_once(|| {
        let _ = env_logger::builder().is_test(true).try_init();
    });
}

/// A private key directory for one `podctl` invocation.
///
/// Every component keeps its own key directory, so a test never lets two
/// components share an identity. Sharing one would silently satisfy every
/// binding check between owner and agent and make the confidentiality boundary
/// untestable, which is exactly the property these tests exist to cover.
pub struct ClientKeyDir {
    directory: tempfile::TempDir,
}

impl ClientKeyDir {
    /// Create a key directory and trust the given agent signing keys.
    ///
    /// `podctl` refuses to deploy to an agent it has not been told to trust, so
    /// a test must state which agents it expects — the same decision an
    /// operator makes.
    pub fn with_trusted_agents(agent_signing_keys: &[String]) -> Result<Self> {
        let directory = tempfile::tempdir().context("create podctl key directory")?;
        std::fs::write(
            directory.path().join(podctl::trust::TRUSTED_AGENTS_FILE),
            agent_signing_keys.join("\n"),
        )
        .context("write trusted agent list")?;
        Ok(Self { directory })
    }

    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    /// Point this process's `podctl` calls at this key directory.
    ///
    /// Tests that use it must run serially, because the variable is process
    /// wide; `#[serial_test::serial]` is the intended companion.
    pub fn activate(&self) {
        // SAFETY: integration tests set this before spawning any component and
        // run serially, so no other thread observes a torn value.
        unsafe { std::env::set_var(podctl::KEY_DIR_ENV_VAR, self.directory.path()) };
    }
}

/// Absolute path of a per-component key directory beneath `root`.
pub fn component_key_dir(root: &Path, component: &str) -> Result<PathBuf> {
    let path = root.join(component);
    std::fs::create_dir_all(&path)
        .with_context(|| format!("create key directory {}", path.display()))?;
    Ok(path)
}

/// Lowest port handed out to a test. Above the range Linux uses for ephemeral
/// ports, so a probe port is never one the kernel might hand to something else.
const TEST_PORT_BASE: u16 = 20_000;
/// Size of the window ports are drawn from.
const TEST_PORT_SPAN: u16 = 20_000;
/// How many candidates to try before giving up.
const MAX_PORT_ATTEMPTS: u16 = 512;

/// Ports already handed out in this process, so two components in one test
/// binary never receive the same one.
fn reserved_ports() -> &'static Mutex<HashSet<u16>> {
    static RESERVED: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    RESERVED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Allocate a port a test may bind.
///
/// Binding port 0 and reading back what the kernel chose is racy in exactly the
/// way that matters here: the listener has to be dropped before the component
/// under test can bind, and `cargo test` runs test binaries in parallel, so
/// another binary can take the port in between. Ports are therefore drawn from
/// a window whose start depends on the process id, and remembered, so parallel
/// binaries scan disjoint regions and a single binary never repeats itself.
fn allocate_port(probe: impl Fn(u16) -> bool) -> u16 {
    let stride = u16::try_from(std::process::id() % u32::from(TEST_PORT_SPAN))
        .expect("modulo keeps this in range");
    let mut reserved = reserved_ports()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    for attempt in 0..MAX_PORT_ATTEMPTS {
        let offset = (stride.wrapping_add(attempt.wrapping_mul(7))) % TEST_PORT_SPAN;
        let port = TEST_PORT_BASE + offset;
        if reserved.contains(&port) || !probe(port) {
            continue;
        }
        reserved.insert(port);
        return port;
    }
    panic!(
        "no free port found in {TEST_PORT_BASE}..{}",
        TEST_PORT_BASE + TEST_PORT_SPAN
    );
}

pub fn allocate_udp_port() -> u16 {
    allocate_port(|port| UdpSocket::bind(("127.0.0.1", port)).is_ok())
}

pub fn allocate_tcp_port() -> u16 {
    allocate_port(|port| TcpListener::bind(("127.0.0.1", port)).is_ok())
}

/// Drops the shared state volume so the stack starts from cold identities.
///
/// The deploy manifests provision every relay keypair, relay token, and Iroh
/// identity themselves on first start, so nothing has to be seeded here. A
/// leftover volume would carry identities that no longer match the freshly
/// generated owner keys, so it is removed instead of reused.
pub async fn reset_podman_stack_state() -> Result<()> {
    if podman_status(&["volume", "exists", PODMESH_STATE_VOLUME]).await? {
        podman_output(&["volume", "rm", "--force", PODMESH_STATE_VOLUME]).await?;
    }
    Ok(())
}

async fn podman_status(args: &[&str]) -> Result<bool> {
    Command::new("podman")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("run podman command")
        .map(|status| status.success())
}

async fn podman_output(args: &[&str]) -> Result<String> {
    let output = Command::new("podman")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("run podman command")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(anyhow!("podman {args:?} failed: {stderr}"));
    }
    Ok(format!("{stdout}\n{stderr}"))
}

/// A tenant owner keypair generated freshly per test.
///
/// Returns `(owner_pub_b64, owner_sk_bytes, owner_pub_bytes)`. Use this to
/// drive the `podctl grant-proxy` flow in integration tests so the proxy
/// holds an owner-signed Biscuit grant and the sidecar can verify it.
pub fn fresh_tenant_owner() -> (String, Vec<u8>, Vec<u8>) {
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    let mut rng = OsRng;
    let sk = SigningKey::generate(&mut rng);
    let pk = sk.verifying_key();
    let pk_bytes = pk.to_bytes().to_vec();
    let sk_bytes = sk.to_bytes().to_vec();
    let pk_b64 = crypto::b64_encode(&pk_bytes);
    (pk_b64, sk_bytes, pk_bytes)
}

/// The credential `podctl` mints at deploy time so a pod can prove its tenancy.
///
/// Without it a sidecar can only claim an owner, which any caller can do, so a
/// proxy refuses to register or tunnel for it.
pub fn workload_credential(owner_sk: &[u8], owner_b64: &str, workload_name: &str) -> String {
    let owner_pk = crypto::b64_decode(owner_b64).expect("decode tenant owner key");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before the unix epoch")
        .as_secs();
    let encoded = protocol::mint_workload_credential(
        owner_sk,
        &owner_pk,
        &protocol::WorkloadCredentialClaims {
            tenant_owner: owner_b64.to_string(),
            manifest_id: protocol::route_id(&owner_pk, workload_name),
            issued_at_secs: now,
            expires_at_secs: now + 3600,
            token_id: format!("credential-{workload_name}"),
        },
        now,
    )
    .expect("mint workload credential");
    protocol::workload_credential_to_b64(&encoded)
}

/// Wait until the proxy REST API at `http://127.0.0.1:{port}/healthz` becomes
/// available, then issue an owner-signed Biscuit grant to it via
/// `podctl::cert::grant_proxy_async`. Returns the issued cert's owner pubkey
/// (base64) on success.
pub async fn provision_proxy_cert(
    rest_port: u16,
    owner_pk: &[u8],
    owner_sk: &[u8],
    timeout: std::time::Duration,
) -> anyhow::Result<podctl::cert::GrantProxyResult> {
    use std::time::Instant;
    let url = format!("http://127.0.0.1:{}", rest_port);
    let client = reqwest::Client::new();
    let deadline = Instant::now() + timeout;
    loop {
        let healthz = format!("{}/healthz", url);
        match client.get(&healthz).send().await {
            Ok(resp) if resp.status().is_success() => break,
            _ => {}
        }
        if Instant::now() >= deadline {
            anyhow::bail!("proxy REST API at {} did not become healthy", url);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let trust_dir = tempfile::tempdir().context("create isolated proxy trust directory")?;
    podctl::cert::trust_proxy_async_at(trust_dir.path(), &url, false)
        .await
        .context("explicitly trust proxy for test")?;
    podctl::cert::grant_proxy_async_at(&url, owner_pk, owner_sk, 30, trust_dir.path()).await
}
