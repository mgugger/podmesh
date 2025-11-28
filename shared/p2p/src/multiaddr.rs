use libp2p::multiaddr::{Multiaddr, Protocol};
use std::net::IpAddr;

/// Build a QUIC multiaddr from host string and port.
pub fn build_quic_multiaddr(host: &str, port: u16) -> Option<Multiaddr> {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ipv4)) => {
            let mut addr = Multiaddr::empty();
            addr.push(Protocol::Ip4(ipv4));
            addr.push(Protocol::Udp(port));
            addr.push(Protocol::QuicV1);
            Some(addr)
        }
        Ok(IpAddr::V6(ipv6)) => {
            let mut addr = Multiaddr::empty();
            addr.push(Protocol::Ip6(ipv6));
            addr.push(Protocol::Udp(port));
            addr.push(Protocol::QuicV1);
            Some(addr)
        }
        Err(_) => format!("/ip4/{}/udp/{}/quic-v1", host, port).parse().ok(),
    }
}

/// Parse a bootstrap peer string (ip:port or multiaddr) into a Multiaddr.
/// Falls back to default_port if no port is specified.
pub fn parse_bootstrap_peer(raw: &str, default_port: u16) -> Option<Multiaddr> {
    // Try parsing as multiaddr first
    if let Ok(ma) = raw.parse::<Multiaddr>() {
        return Some(ma);
    }

    // Parse as host:port
    let (host, port) = match raw.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(port) => (h, port),
            Err(_) => {
                log::warn!("invalid bootstrap port in '{}'", raw);
                return None;
            }
        },
        None => (raw, default_port),
    };

    if port == 0 && default_port == 0 {
        log::warn!("bootstrap address '{}' missing port and no default provided", raw);
        return None;
    }

    let effective_port = if port == 0 { default_port } else { port };
    build_quic_multiaddr(host, effective_port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_quic_multiaddr_ipv4() {
        let addr = build_quic_multiaddr("127.0.0.1", 4001).unwrap();
        assert!(addr.to_string().contains("/ip4/127.0.0.1"));
        assert!(addr.to_string().contains("/udp/4001"));
        assert!(addr.to_string().contains("/quic-v1"));
    }

    #[test]
    fn test_build_quic_multiaddr_ipv6() {
        let addr = build_quic_multiaddr("::1", 4001).unwrap();
        assert!(addr.to_string().contains("/ip6/::1"));
    }

    #[test]
    fn test_parse_bootstrap_peer_multiaddr() {
        let addr = parse_bootstrap_peer("/ip4/127.0.0.1/udp/4001/quic-v1", 0).unwrap();
        assert!(addr.to_string().contains("127.0.0.1"));
    }

    #[test]
    fn test_parse_bootstrap_peer_host_port() {
        let addr = parse_bootstrap_peer("192.168.1.1:5000", 4001).unwrap();
        assert!(addr.to_string().contains("192.168.1.1"));
        assert!(addr.to_string().contains("5000"));
    }

    #[test]
    fn test_parse_bootstrap_peer_host_only() {
        let addr = parse_bootstrap_peer("192.168.1.1", 4001).unwrap();
        assert!(addr.to_string().contains("4001"));
    }
}
