use std::collections::HashSet;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::capacity::{
    MAX_CAPACITY_CAPABILITIES, MAX_CAPACITY_CAPABILITY_LEN, MAX_CAPACITY_EXCLUDED_ENDPOINTS,
    MAX_CAPACITY_ID_LEN, MAX_CAPACITY_MESSAGE_BYTES,
};
use crate::{CAPACITY_PROTOCOL_VERSION, CapacityOffer, IROH_ENDPOINT_ID_BYTES};

pub const PLACEMENT_PROTOCOL_VERSION: u16 = 1;
pub const MAX_PLACEMENT_REQUEST_LIFETIME_SECS: u64 = 15;
pub const MAX_PLACEMENT_CLOCK_SKEW_SECS: u64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlacementRequest {
    pub version: u16,
    pub request_id: String,
    pub cpu_milli: u32,
    pub memory_bytes: u64,
    pub storage_bytes: u64,
    pub required_capabilities: Vec<String>,
    pub excluded_endpoint_ids: Vec<Vec<u8>>,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
}

impl PlacementRequest {
    pub fn to_bytes(&self, now_secs: u64) -> Result<Vec<u8>> {
        self.validate(now_secs)?;
        encode_bounded(self, "placement request")
    }

    pub fn from_bytes(bytes: &[u8], now_secs: u64) -> Result<Self> {
        validate_size(bytes)?;
        let request: Self = postcard::from_bytes(bytes).context("decode placement request")?;
        request.validate(now_secs)?;
        Ok(request)
    }

    fn validate(&self, now_secs: u64) -> Result<()> {
        ensure!(
            self.version == PLACEMENT_PROTOCOL_VERSION,
            "unsupported placement protocol version"
        );
        ensure!(
            !self.request_id.is_empty() && self.request_id.len() <= MAX_CAPACITY_ID_LEN,
            "placement request ID length is invalid"
        );
        ensure!(
            self.cpu_milli > 0 && self.memory_bytes > 0 && self.storage_bytes > 0,
            "placement resources must be non-zero"
        );
        ensure!(
            self.required_capabilities.len() <= MAX_CAPACITY_CAPABILITIES,
            "too many placement capabilities"
        );
        let mut capabilities = HashSet::with_capacity(self.required_capabilities.len());
        for capability in &self.required_capabilities {
            ensure!(
                !capability.is_empty() && capability.len() <= MAX_CAPACITY_CAPABILITY_LEN,
                "placement capability length is invalid"
            );
            ensure!(
                capabilities.insert(capability),
                "duplicate placement capability"
            );
        }
        ensure!(
            self.excluded_endpoint_ids.len() <= MAX_CAPACITY_EXCLUDED_ENDPOINTS,
            "too many excluded placement endpoints"
        );
        let mut exclusions = HashSet::with_capacity(self.excluded_endpoint_ids.len());
        for endpoint_id in &self.excluded_endpoint_ids {
            ensure!(
                endpoint_id.len() == IROH_ENDPOINT_ID_BYTES,
                "excluded placement EndpointId must contain 32 bytes"
            );
            ensure!(
                exclusions.insert(endpoint_id),
                "duplicate excluded placement endpoint"
            );
        }
        ensure!(
            self.issued_at_secs <= now_secs.saturating_add(MAX_PLACEMENT_CLOCK_SKEW_SECS),
            "placement request issue time is too far in the future"
        );
        ensure!(
            self.expires_at_secs >= now_secs,
            "placement request expired"
        );
        ensure!(
            self.expires_at_secs >= self.issued_at_secs
                && self.expires_at_secs - self.issued_at_secs
                    <= MAX_PLACEMENT_REQUEST_LIFETIME_SECS,
            "placement request lifetime is invalid"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PlacementError {
    NoCapacity,
    Busy,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlacementResponse {
    pub version: u16,
    pub request_id: String,
    pub offer: Option<CapacityOffer>,
    pub error: Option<PlacementError>,
}

impl PlacementResponse {
    pub fn selected(request_id: String, offer: CapacityOffer) -> Self {
        Self {
            version: PLACEMENT_PROTOCOL_VERSION,
            request_id,
            offer: Some(offer),
            error: None,
        }
    }

    pub fn failed(request_id: String, error: PlacementError) -> Self {
        Self {
            version: PLACEMENT_PROTOCOL_VERSION,
            request_id,
            offer: None,
            error: Some(error),
        }
    }

    pub fn to_bytes(&self, now_secs: u64) -> Result<Vec<u8>> {
        self.validate(now_secs)?;
        encode_bounded(self, "placement response")
    }

    pub fn from_bytes(bytes: &[u8], now_secs: u64) -> Result<Self> {
        validate_size(bytes)?;
        let response: Self = postcard::from_bytes(bytes).context("decode placement response")?;
        response.validate(now_secs)?;
        Ok(response)
    }

    fn validate(&self, now_secs: u64) -> Result<()> {
        ensure!(
            self.version == PLACEMENT_PROTOCOL_VERSION,
            "unsupported placement response version"
        );
        ensure!(
            !self.request_id.is_empty() && self.request_id.len() <= MAX_CAPACITY_ID_LEN,
            "placement response ID length is invalid"
        );
        ensure!(
            self.offer.is_some() ^ self.error.is_some(),
            "placement response must contain exactly one result"
        );
        if let Some(offer) = &self.offer {
            ensure!(
                offer.version == CAPACITY_PROTOCOL_VERSION,
                "invalid placement offer version"
            );
            offer.verify(now_secs)?;
        }
        Ok(())
    }
}

fn encode_bounded(value: &impl Serialize, field: &str) -> Result<Vec<u8>> {
    let bytes = postcard::to_allocvec(value).with_context(|| format!("serialize {field}"))?;
    validate_size(&bytes)?;
    Ok(bytes)
}

fn validate_size(bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_CAPACITY_MESSAGE_BYTES,
        "placement message encoded size is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000;

    fn request() -> PlacementRequest {
        PlacementRequest {
            version: PLACEMENT_PROTOCOL_VERSION,
            request_id: "request-1".into(),
            cpu_milli: 500,
            memory_bytes: 512 * 1024 * 1024,
            storage_bytes: 1024 * 1024 * 1024,
            required_capabilities: vec!["linux".into()],
            excluded_endpoint_ids: vec![vec![7; IROH_ENDPOINT_ID_BYTES]],
            issued_at_secs: NOW,
            expires_at_secs: NOW + 5,
        }
    }

    #[test]
    fn placement_request_and_error_response_roundtrip() {
        let request = request();
        assert_eq!(
            PlacementRequest::from_bytes(&request.to_bytes(NOW).unwrap(), NOW).unwrap(),
            request
        );
        let response = PlacementResponse::failed(request.request_id, PlacementError::NoCapacity);
        assert_eq!(
            PlacementResponse::from_bytes(&response.to_bytes(NOW).unwrap(), NOW).unwrap(),
            response
        );
    }

    #[test]
    fn expired_duplicate_and_malformed_requests_fail_closed() {
        let mut expired = request();
        expired.expires_at_secs = NOW - 1;
        assert!(expired.to_bytes(NOW).is_err());

        let mut duplicate = request();
        duplicate.required_capabilities.push("linux".into());
        assert!(duplicate.to_bytes(NOW).is_err());

        let mut malformed = request();
        malformed.excluded_endpoint_ids = vec![vec![1; IROH_ENDPOINT_ID_BYTES - 1]];
        assert!(malformed.to_bytes(NOW).is_err());
    }

    #[test]
    fn response_requires_exactly_one_result() {
        let response = PlacementResponse {
            version: PLACEMENT_PROTOCOL_VERSION,
            request_id: "request-1".into(),
            offer: None,
            error: None,
        };
        assert!(response.to_bytes(NOW).is_err());
    }
}
