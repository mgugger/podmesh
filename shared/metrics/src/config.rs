#![forbid(unsafe_code)]

use std::{net::SocketAddr, time::Duration};

pub const MAX_CONNECTIONS: usize = 32;
pub const MAX_CONCURRENT_REQUESTS: usize = 8;
pub const MAX_REQUEST_BYTES: usize = 8 * 1024;
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
pub const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MetricsConfig {
    pub listen: Option<SocketAddr>,
}

pub fn listeners_conflict(left: SocketAddr, right: SocketAddr) -> bool {
    left.port() == right.port()
        && (left.ip() == right.ip() || left.ip().is_unspecified() || right.ip().is_unspecified())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_and_specific_addresses_on_one_port_conflict() {
        assert!(listeners_conflict(
            "0.0.0.0:9200".parse().unwrap(),
            "127.0.0.1:9200".parse().unwrap(),
        ));
        assert!(!listeners_conflict(
            "127.0.0.1:9200".parse().unwrap(),
            "127.0.0.1:9201".parse().unwrap(),
        ));
        assert!(!listeners_conflict(
            "127.0.0.1:9200".parse().unwrap(),
            "127.0.0.2:9200".parse().unwrap(),
        ));
    }
}
