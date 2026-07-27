use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const MAX_PROXY_PEERS: usize = 32;
pub const MAX_PROXY_ADDRS_PER_PEER: usize = 8;
pub const MAX_PROXY_PEER_ID_LEN: usize = 128;
pub const MAX_PROXY_MULTIADDR_LEN: usize = 512;
pub const MAX_OWNER_PUBKEY_B64_LEN: usize = 128;
pub const MAX_PROXY_DISCOVERY_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ProxyPeer {
    pub peer_id: String,
    pub addresses: Vec<String>,
}

impl ProxyPeer {
    pub fn validate(&self) -> Result<()> {
        if self.peer_id.is_empty() || self.peer_id.len() > MAX_PROXY_PEER_ID_LEN {
            bail!("proxy peer ID length is invalid");
        }
        if self.addresses.is_empty() || self.addresses.len() > MAX_PROXY_ADDRS_PER_PEER {
            bail!("proxy peer address count is invalid");
        }
        let expected_suffix = format!("/p2p/{}", self.peer_id);
        for address in &self.addresses {
            if address.is_empty() || address.len() > MAX_PROXY_MULTIADDR_LEN {
                bail!("proxy peer multiaddr length is invalid");
            }
            if !address.ends_with(&expected_suffix) {
                bail!("proxy peer multiaddr is not bound to its peer ID");
            }
        }
        Ok(())
    }
}

pub fn validate_proxy_peers(peers: &[ProxyPeer], allow_empty: bool) -> Result<()> {
    if (!allow_empty && peers.is_empty()) || peers.len() > MAX_PROXY_PEERS {
        bail!("proxy peer count is invalid");
    }
    for peer in peers {
        peer.validate()?;
    }
    Ok(())
}

pub fn proxy_peers_from_multiaddrs(addresses: &[String]) -> Result<Vec<ProxyPeer>> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for address in addresses {
        let (prefix, peer_id) = address
            .rsplit_once("/p2p/")
            .ok_or_else(|| anyhow::anyhow!("proxy multiaddr must end in /p2p/<peer-id>"))?;
        if prefix.is_empty() || peer_id.is_empty() || peer_id.contains('/') {
            bail!("proxy multiaddr has invalid peer ID suffix");
        }
        grouped
            .entry(peer_id.to_string())
            .or_default()
            .push(address.clone());
    }
    let peers: Vec<ProxyPeer> = grouped
        .into_iter()
        .map(|(peer_id, addresses)| ProxyPeer { peer_id, addresses })
        .collect();
    validate_proxy_peers(&peers, false)?;
    Ok(peers)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyDiscoveryRequest {
    pub owner_pubkey: String,
    pub limit: u16,
}

impl ProxyDiscoveryRequest {
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        postcard::to_allocvec(self).context("serialize proxy discovery request")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        validate_message_size(bytes)?;
        let request: Self =
            postcard::from_bytes(bytes).context("decode proxy discovery request")?;
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<()> {
        if self.owner_pubkey.is_empty() || self.owner_pubkey.len() > MAX_OWNER_PUBKEY_B64_LEN {
            bail!("owner public key length is invalid");
        }
        if self.limit == 0 || usize::from(self.limit) > MAX_PROXY_PEERS {
            bail!("proxy discovery limit is invalid");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyDiscoveryResponse {
    pub peers: Vec<ProxyPeer>,
}

impl ProxyDiscoveryResponse {
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        validate_proxy_peers(&self.peers, true)?;
        let bytes = postcard::to_allocvec(self).context("serialize proxy discovery response")?;
        validate_message_size(&bytes)?;
        Ok(bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        validate_message_size(bytes)?;
        let response: Self =
            postcard::from_bytes(bytes).context("decode proxy discovery response")?;
        validate_proxy_peers(&response.peers, true)?;
        Ok(response)
    }
}

fn validate_message_size(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_PROXY_DISCOVERY_MESSAGE_BYTES {
        bail!("proxy discovery message exceeds size limit");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER_ID: &str = "12D3KooWJ5WjY5GLqvC7V7abCdzYdHEgQmXW1HYXL7rGZQfJmY9D";

    fn valid_peer() -> ProxyPeer {
        ProxyPeer {
            peer_id: PEER_ID.to_string(),
            addresses: vec![format!("/ip4/192.0.2.1/udp/4002/quic-v1/p2p/{PEER_ID}")],
        }
    }

    #[test]
    fn discovery_roundtrip() {
        let response = ProxyDiscoveryResponse {
            peers: vec![valid_peer()],
        };
        let decoded = ProxyDiscoveryResponse::from_bytes(&response.to_bytes().unwrap()).unwrap();
        assert_eq!(decoded, response);
    }

    #[test]
    fn rejects_address_bound_to_another_peer() {
        let mut peer = valid_peer();
        peer.addresses[0] = "/ip4/192.0.2.1/udp/4002/quic-v1/p2p/other".to_string();
        assert!(peer.validate().is_err());
    }

    #[test]
    fn rejects_unbounded_peer_response() {
        let response = ProxyDiscoveryResponse {
            peers: vec![valid_peer(); MAX_PROXY_PEERS + 1],
        };
        assert!(response.to_bytes().is_err());
    }

    #[test]
    fn rejects_invalid_request_limit() {
        let request = ProxyDiscoveryRequest {
            owner_pubkey: "owner".to_string(),
            limit: 0,
        };
        assert!(request.to_bytes().is_err());
    }

    #[test]
    fn groups_addresses_by_peer_id() {
        let addresses = vec![
            format!("/ip4/192.0.2.1/udp/4002/quic-v1/p2p/{PEER_ID}"),
            format!("/ip6/2001:db8::1/udp/4002/quic-v1/p2p/{PEER_ID}"),
        ];
        let peers = proxy_peers_from_multiaddrs(&addresses).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].addresses, addresses);
    }
}
