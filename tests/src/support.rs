use std::net::{TcpListener, UdpSocket};
use std::sync::Once;

static INIT_LOGGING: Once = Once::new();
static INIT_EPHEMERAL_KEYS: Once = Once::new();

pub fn init_tracing() {
    INIT_LOGGING.call_once(|| {
        let _ = env_logger::builder()
            .is_test(true)
            .try_init();
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

/// Generate a NodeCert for testing, signed by the ephemeral owner key.
/// peer_id: the peer's string ID
/// role: the node role
pub fn generate_test_node_cert(peer_id: &str, role: protocol::NodeRole) -> protocol::NodeCert {
    use crypto::ensure_keypair_ephemeral;

    let (pk, sk) = ensure_keypair_ephemeral().unwrap();
    let (kem_pk, _) = ensure_keypair_ephemeral().unwrap();

    let cert = protocol::NodeCert {
        peer_id: peer_id.to_string(),
        kem_pubkey: crypto::b64_encode(&kem_pk),
        signing_pubkey: crypto::b64_encode(&pk),
        capabilities: vec!["test".to_string()],
        role,
        valid_until: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 86400 * 365,
        owner_pubkey: crypto::b64_encode(&pk),
        owner_sig: String::new(),
        endorsements: vec![],
    };
    cert.sign(&sk, &pk).unwrap()
}

/// A tenant owner keypair generated freshly per test.
///
/// Returns `(owner_pub_b64, owner_sk_bytes, owner_pub_bytes)`. Use this to
/// drive the `podctl grant-proxy` flow in integration tests so the proxy
/// holds a tenant-signed `NodeCert` and the sidecar can verify it.
pub fn fresh_tenant_owner() -> (String, Vec<u8>, Vec<u8>) {
    use rand::rngs::OsRng;
    use ed25519_dalek::SigningKey;
    let mut rng = OsRng;
    let sk = SigningKey::generate(&mut rng);
    let pk = sk.verifying_key();
    let pk_bytes = pk.to_bytes().to_vec();
    let sk_bytes = sk.to_bytes().to_vec();
    let pk_b64 = crypto::b64_encode(&pk_bytes);
    (pk_b64, sk_bytes, pk_bytes)
}

/// Wait until the proxy REST API at `http://127.0.0.1:{port}/healthz` becomes
/// available, then issue a tenant-signed `NodeCert` to it via
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
