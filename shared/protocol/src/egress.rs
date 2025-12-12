//! Egress tunnel protocol types for transparent proxy support.
//!
//! This module defines the message types used for tunneling outbound connections
//! from sidecar containers through proxy nodes to external destinations.

use serde::{Deserialize, Serialize};

/// Default port for the sidecar's transparent proxy listener.
pub const EGRESS_PROXY_PORT: u16 = 15001;

/// Networks to exclude from egress interception (pod-local traffic).
/// These are CIDR ranges that should not be redirected through the tunnel.
pub const EGRESS_EXCLUDE_NETWORKS: &[&str] = &[
    "127.0.0.0/8",   // Loopback
    "10.0.2.0/24",   // Pasta/slirp4netns default network
    "169.254.0.0/16", // Link-local
];

/// Protocol type for egress tunneling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EgressProtocol {
    /// TCP protocol
    Tcp,
    /// UDP protocol (future support)
    Udp,
}

impl Default for EgressProtocol {
    fn default() -> Self {
        Self::Tcp
    }
}

impl std::fmt::Display for EgressProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => write!(f, "tcp"),
            Self::Udp => write!(f, "udp"),
        }
    }
}

/// Request to establish an egress tunnel to an external destination.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EgressTunnelRequest {
    /// Target host (IP address or hostname).
    pub target_host: String,
    /// Target port number.
    pub target_port: u16,
    /// Protocol type (currently only "tcp" is supported).
    pub protocol: String,
}

impl EgressTunnelRequest {
    /// Create a new TCP egress tunnel request.
    pub fn tcp(host: impl Into<String>, port: u16) -> Self {
        Self {
            target_host: host.into(),
            target_port: port,
            protocol: "tcp".to_string(),
        }
    }
}

/// Response indicating whether the egress tunnel was established.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EgressTunnelResponse {
    /// Whether the connection was successfully established.
    pub success: bool,
    /// Error message if the connection failed.
    pub error: Option<String>,
}

impl EgressTunnelResponse {
    /// Create a successful response.
    pub fn ok() -> Self {
        Self {
            success: true,
            error: None,
        }
    }

    /// Create an error response.
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(message.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_egress_request_tcp() {
        let req = EgressTunnelRequest::tcp("example.com", 443);
        assert_eq!(req.target_host, "example.com");
        assert_eq!(req.target_port, 443);
        assert_eq!(req.protocol, "tcp");
    }

    #[test]
    fn test_egress_response_ok() {
        let resp = EgressTunnelResponse::ok();
        assert!(resp.success);
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_egress_response_err() {
        let resp = EgressTunnelResponse::err("connection refused");
        assert!(!resp.success);
        assert_eq!(resp.error, Some("connection refused".to_string()));
    }
}
