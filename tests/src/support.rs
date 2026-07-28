use std::net::{TcpListener, UdpSocket};
use std::process::Stdio;
use std::sync::Once;

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;

static INIT_LOGGING: Once = Once::new();
static INIT_EPHEMERAL_KEYS: Once = Once::new();
/// Single volume the deploy manifests mount into every component.
const PODMESH_STATE_VOLUME: &str = "podmesh-state";

pub fn init_tracing() {
    INIT_LOGGING.call_once(|| {
        let _ = env_logger::builder().is_test(true).try_init();
    });
}

/// Initialize ephemeral keypairs for integration tests.
///
/// This switches the crypto crate to ephemeral mode, avoiding file-based
/// key storage. This prevents race conditions when multiple test processes
/// run concurrently and try to read/write the same key files.
///
/// Call this at the start of each test that uses P2P communication.
pub fn init_ephemeral_keys() {
    INIT_EPHEMERAL_KEYS.call_once(|| {
        crypto::set_keypair_config(crypto::KeypairConfig {
            signing_mode: crypto::KeypairMode::Ephemeral,
            kem_mode: crypto::KeypairMode::Ephemeral,
            key_directory: None,
        });
    });
}

pub fn allocate_udp_port() -> u16 {
    UdpSocket::bind(("127.0.0.1", 0))
        .expect("bind udp port")
        .local_addr()
        .expect("udp local addr")
        .port()
}

pub fn allocate_tcp_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind tcp port")
        .local_addr()
        .expect("tcp local addr")
        .port()
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
    podctl::cert::grant_proxy_async(&url, owner_pk, owner_sk, 30).await
}
