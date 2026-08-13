//! Resolving an egress tunnel destination.
//!
//! The proxy dials arbitrary destinations on behalf of workloads. Which
//! destinations are allowed is deliberately *not* decided here by address:
//! podmesh is deployed across machines whose app parts legitimately live on
//! private networks, so an address filter would block real traffic while doing
//! nothing about the threat that matters. A tunnel is authorised by tenant
//! instead — the proxy must hold a live owner grant for the tenant that asked —
//! which is enforced by the caller before resolution is ever attempted.
//!
//! Resolution still happens exactly once, and the caller dials the addresses
//! returned here, so a name cannot resolve to one host for a check and another
//! for the connection.

use std::net::{SocketAddr, ToSocketAddrs};

use anyhow::{Result, ensure};

/// Longest DNS name an egress request may name.
pub const MAX_EGRESS_HOST_LEN: usize = 253;

/// Resolve `host:port` into the addresses a tunnel may dial.
pub fn resolve_target(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    ensure!(!host.is_empty(), "egress target host is empty");
    ensure!(
        host.len() <= MAX_EGRESS_HOST_LEN,
        "egress target host exceeds {MAX_EGRESS_HOST_LEN} bytes"
    );
    ensure!(port != 0, "egress target port must be non-zero");

    let resolved: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|error| anyhow::anyhow!("resolve egress target {host}: {error}"))?
        .collect();
    ensure!(!resolved.is_empty(), "egress target {host} did not resolve");
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resolvable_target_yields_addresses_to_dial() {
        let resolved = resolve_target("127.0.0.1", 7100).expect("loopback resolves");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].port(), 7100);
    }

    #[test]
    fn malformed_targets_are_refused_before_resolution() {
        assert!(resolve_target("", 80).is_err());
        assert!(resolve_target("example.com", 0).is_err());
        assert!(resolve_target(&"a".repeat(MAX_EGRESS_HOST_LEN + 1), 80).is_err());
    }
}
