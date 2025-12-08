use std::net::{TcpListener, UdpSocket};
use std::sync::Once;

static INIT_LOGGING: Once = Once::new();

pub fn init_tracing() {
    INIT_LOGGING.call_once(|| {
        let _ = env_logger::builder()
            .is_test(true)
            .try_init();
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
