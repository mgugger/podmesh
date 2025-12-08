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
