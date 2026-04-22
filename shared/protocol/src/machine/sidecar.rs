use anyhow::Context;
use serde::{Deserialize, Serialize};

fn serialize<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("sidecar serialization should succeed")
}

fn deserialize<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[repr(u8)]
pub enum SidecarRouteKind {
    Service = 0,
    Ingress = 1,
}

impl Default for SidecarRouteKind {
    fn default() -> Self {
        SidecarRouteKind::Service
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SidecarRouteWire {
    path_prefix: String,
    target_port: u16,
    service_name: String,
    service_port: String,
    source: SidecarRouteKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SidecarProviderRecordWire {
    manifest_id: String,
    peer_id: String,
    host: String,
    owner_public_key_b64: String,
    routes: Vec<SidecarRouteWire>,
    ttl_ms: u32,
    last_updated_ms: u64,
    version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarRouteSpec {
    pub host: String,
    pub path_prefix: String,
    pub target_port: u16,
    pub service_name: String,
    pub service_port: String,
    pub source: SidecarRouteKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarProviderRecordOwned {
    pub manifest_id: String,
    pub peer_id: String,
    pub host: String,
    pub owner_public_key_b64: Option<String>,
    pub routes: Vec<SidecarRouteSpec>,
    pub ttl_ms: u32,
    pub last_updated_ms: u64,
    pub version: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SidecarManifestRequest {
    pub manifest_id: String,
}

impl SidecarManifestRequest {
    pub fn to_bytes(&self) -> Vec<u8> {
        serialize(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        deserialize(bytes)
    }
}

pub fn build_sidecar_provider_record(
    manifest_id: &str,
    peer_id: &str,
    host: &str,
    owner_public_key_b64: Option<&str>,
    routes: &[SidecarRouteSpec],
    ttl_ms: u32,
    last_updated_ms: u64,
    version: u16,
) -> Vec<u8> {
    let wire = SidecarProviderRecordWire {
        manifest_id: manifest_id.to_string(),
        peer_id: peer_id.to_string(),
        host: host.to_string(),
        owner_public_key_b64: owner_public_key_b64.unwrap_or_default().to_string(),
        routes: routes
            .iter()
            .map(|route| SidecarRouteWire {
                path_prefix: route.path_prefix.clone(),
                target_port: route.target_port,
                service_name: route.service_name.clone(),
                service_port: route.service_port.clone(),
                source: route.source,
            })
            .collect(),
        ttl_ms,
        last_updated_ms,
        version,
    };
    serialize(&wire)
}

pub fn root_as_sidecar_provider_record(
    bytes: &[u8],
) -> Result<SidecarProviderRecordWire, postcard::Error> {
    deserialize(bytes)
}

pub fn decode_sidecar_provider_record(data: &[u8]) -> anyhow::Result<SidecarProviderRecordOwned> {
    let record =
        root_as_sidecar_provider_record(data).context("failed to parse sidecar provider record")?;

    let SidecarProviderRecordWire {
        manifest_id,
        peer_id,
        host,
        owner_public_key_b64,
        routes,
        ttl_ms,
        last_updated_ms,
        version,
    } = record;

    let owner_public_key_b64 = if owner_public_key_b64.trim().is_empty() {
        None
    } else {
        Some(owner_public_key_b64)
    };

    let routes = routes
        .into_iter()
        .map(|route| SidecarRouteSpec {
            host: host.clone(),
            path_prefix: route.path_prefix,
            target_port: route.target_port,
            service_name: route.service_name,
            service_port: route.service_port,
            source: route.source,
        })
        .collect();

    Ok(SidecarProviderRecordOwned {
        manifest_id,
        peer_id,
        host,
        owner_public_key_b64,
        routes,
        ttl_ms,
        last_updated_ms,
        version,
    })
}

pub fn build_sidecar_manifest_request(manifest_id: &str) -> Vec<u8> {
    serialize(&SidecarManifestRequest {
        manifest_id: manifest_id.to_string(),
    })
}

pub fn root_as_sidecar_manifest_request(
    bytes: &[u8],
) -> Result<SidecarManifestRequest, postcard::Error> {
    deserialize(bytes)
}
