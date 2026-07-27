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
}
