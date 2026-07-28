use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::EndpointRecord;

pub const MAX_PROXY_ENDPOINTS: usize = 32;
pub const MAX_OWNER_PUBKEY_B64_LEN: usize = 128;
pub const MAX_PROXY_DISCOVERY_MESSAGE_BYTES: usize = 64 * 1024;

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
        if self.limit == 0 || usize::from(self.limit) > MAX_PROXY_ENDPOINTS {
            bail!("proxy discovery limit is invalid");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyEndpointDiscoveryResponse {
    pub endpoints: Vec<EndpointRecord>,
}

impl ProxyEndpointDiscoveryResponse {
    pub fn to_bytes(&self, now_secs: u64) -> Result<Vec<u8>> {
        validate_endpoint_records(&self.endpoints, now_secs)?;
        let bytes = postcard::to_allocvec(self).context("serialize proxy endpoint discovery")?;
        validate_message_size(&bytes)?;
        Ok(bytes)
    }

    pub fn from_bytes(bytes: &[u8], now_secs: u64) -> Result<Self> {
        validate_message_size(bytes)?;
        let response: Self =
            postcard::from_bytes(bytes).context("decode proxy endpoint discovery")?;
        validate_endpoint_records(&response.endpoints, now_secs)?;
        Ok(response)
    }
}

fn validate_endpoint_records(records: &[EndpointRecord], now_secs: u64) -> Result<()> {
    if records.len() > MAX_PROXY_ENDPOINTS {
        bail!("proxy endpoint count is invalid");
    }
    for record in records {
        record.verify(now_secs)?;
    }
    Ok(())
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

    #[test]
    fn rejects_invalid_request_limit() {
        let request = ProxyDiscoveryRequest {
            owner_pubkey: "owner".into(),
            limit: 0,
        };
        assert!(request.to_bytes().is_err());
    }
}
