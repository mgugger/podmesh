use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Handshake {
    pub nonce: u32,
    pub timestamp: u64,
    pub protocol_version: String,
    pub signature: String,
    pub proxy_cert_b64: String,
    pub tenant_owner_pubkey: String,
}

impl Handshake {
    pub fn nonce(&self) -> u32 {
        self.nonce
    }
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }
    pub fn protocol_version(&self) -> Option<&str> {
        non_empty(&self.protocol_version)
    }
    pub fn signature(&self) -> Option<&str> {
        non_empty(&self.signature)
    }
    pub fn proxy_cert_b64(&self) -> Option<&str> {
        non_empty(&self.proxy_cert_b64)
    }
    pub fn tenant_owner_pubkey(&self) -> Option<&str> {
        non_empty(&self.tenant_owner_pubkey)
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("handshake serialization should succeed")
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

fn non_empty(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}
