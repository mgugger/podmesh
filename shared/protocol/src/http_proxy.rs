use serde::{Deserialize, Serialize};

use anyhow::{Result, ensure};

pub const MAX_HTTP_METHOD_BYTES: usize = 32;
pub const MAX_HTTP_PATH_BYTES: usize = 8 * 1024;
pub const MAX_HTTP_HEADERS: usize = 128;
pub const MAX_HTTP_HEADER_NAME_BYTES: usize = 256;
pub const MAX_HTTP_HEADER_VALUE_BYTES: usize = 8 * 1024;
pub const MAX_HTTP_HEADERS_BYTES: usize = 48 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngressRequestMetadata {
    pub manifest_id: String,
    pub method: String,
    pub path_and_query: String,
    pub headers: Vec<(String, String)>,
    pub target_port: u16,
    pub upgrade_requested: bool,
}

impl IngressRequestMetadata {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.manifest_id.is_empty() && self.manifest_id.len() <= 128,
            "ingress manifest id is invalid"
        );
        ensure!(
            !self.method.is_empty()
                && self.method.len() <= MAX_HTTP_METHOD_BYTES
                && self.method.is_ascii(),
            "ingress method is invalid"
        );
        ensure!(
            !self.path_and_query.is_empty() && self.path_and_query.len() <= MAX_HTTP_PATH_BYTES,
            "ingress path and query is invalid"
        );
        validate_headers(&self.headers)
    }
}

impl crate::WorkloadPayload for IngressRequestMetadata {
    const TYPE: crate::WorkloadPayloadType = crate::WorkloadPayloadType::IngressRequest;

    fn validate(&self, _now_secs: u64) -> Result<()> {
        self.validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngressResponseMetadata {
    pub status_code: u16,
    pub headers: Vec<(String, String)>,
    pub upgrade_accepted: bool,
}

impl IngressResponseMetadata {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (100..=599).contains(&self.status_code),
            "ingress response status is invalid"
        );
        ensure!(
            !self.upgrade_accepted || self.status_code == 101,
            "ingress upgrade requires status 101"
        );
        validate_headers(&self.headers)
    }
}

impl crate::WorkloadPayload for IngressResponseMetadata {
    const TYPE: crate::WorkloadPayloadType = crate::WorkloadPayloadType::IngressResponse;

    fn validate(&self, _now_secs: u64) -> Result<()> {
        self.validate()
    }
}

fn validate_headers(headers: &[(String, String)]) -> Result<()> {
    ensure!(
        headers.len() <= MAX_HTTP_HEADERS,
        "too many ingress headers"
    );
    let mut total = 0usize;
    for (name, value) in headers {
        ensure!(
            !name.is_empty() && name.len() <= MAX_HTTP_HEADER_NAME_BYTES && name.is_ascii(),
            "ingress header name is invalid"
        );
        ensure!(
            value.len() <= MAX_HTTP_HEADER_VALUE_BYTES,
            "ingress header value exceeds its limit"
        );
        total = total
            .checked_add(name.len() + value.len())
            .ok_or_else(|| anyhow::anyhow!("ingress header size overflow"))?;
    }
    ensure!(
        total <= MAX_HTTP_HEADERS_BYTES,
        "combined ingress headers exceed their limit"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyHttpRequest {
    pub manifest_id: String,
    pub method: String,
    pub path_and_query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub target_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyHttpResponse {
    pub status_code: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_acceptance_requires_status_101() {
        let invalid = IngressResponseMetadata {
            status_code: 200,
            headers: Vec::new(),
            upgrade_accepted: true,
        };
        assert!(invalid.validate().is_err());
        let valid = IngressResponseMetadata {
            status_code: 101,
            headers: vec![("connection".into(), "upgrade".into())],
            upgrade_accepted: true,
        };
        valid.validate().unwrap();
    }
}
