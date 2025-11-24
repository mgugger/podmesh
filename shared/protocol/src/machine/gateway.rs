use anyhow::Context;
use serde::{Deserialize, Serialize};

fn serialize<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("gateway serialization should succeed")
}

fn deserialize<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[repr(u8)]
pub enum GatewayRouteKind {
    Service = 0,
    Ingress = 1,
}

impl Default for GatewayRouteKind {
    fn default() -> Self {
        GatewayRouteKind::Service
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct GatewayRouteWire {
    path_prefix: String,
    target_port: u16,
    service_name: String,
    service_port: String,
    source: GatewayRouteKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayProviderRecordWire {
    manifest_id: String,
    peer_id: String,
    host: String,
    owner_public_key_b64: String,
    routes: Vec<GatewayRouteWire>,
    ttl_ms: u32,
    last_updated_ms: u64,
    version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayRouteSpec {
    pub host: String,
    pub path_prefix: String,
    pub target_port: u16,
    pub service_name: String,
    pub service_port: String,
    pub source: GatewayRouteKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayProviderRecordOwned {
    pub manifest_id: String,
    pub peer_id: String,
    pub host: String,
    pub owner_public_key_b64: Option<String>,
    pub routes: Vec<GatewayRouteSpec>,
    pub ttl_ms: u32,
    pub last_updated_ms: u64,
    pub version: u16,
}

pub fn build_gateway_provider_record(
    manifest_id: &str,
    peer_id: &str,
    host: &str,
    owner_public_key_b64: Option<&str>,
    routes: &[GatewayRouteSpec],
    ttl_ms: u32,
    last_updated_ms: u64,
    version: u16,
) -> Vec<u8> {
    let wire = GatewayProviderRecordWire {
        manifest_id: manifest_id.to_string(),
        peer_id: peer_id.to_string(),
        host: host.to_string(),
        owner_public_key_b64: owner_public_key_b64.unwrap_or_default().to_string(),
        routes: routes
            .iter()
            .map(|route| GatewayRouteWire {
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

pub fn root_as_gateway_provider_record(
    bytes: &[u8],
) -> Result<GatewayProviderRecordWire, postcard::Error> {
    deserialize(bytes)
}

pub fn decode_gateway_provider_record(data: &[u8]) -> anyhow::Result<GatewayProviderRecordOwned> {
    let record =
        root_as_gateway_provider_record(data).context("failed to parse gateway provider record")?;

    let GatewayProviderRecordWire {
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
        .map(|route| GatewayRouteSpec {
            host: host.clone(),
            path_prefix: route.path_prefix,
            target_port: route.target_port,
            service_name: route.service_name,
            service_port: route.service_port,
            source: route.source,
        })
        .collect();

    Ok(GatewayProviderRecordOwned {
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
