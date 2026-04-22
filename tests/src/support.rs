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
