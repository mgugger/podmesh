use anyhow::{Context, Result};
use iroh::{EndpointAddr, EndpointId};
use protocol::EndpointRecord;

pub fn endpoint_addr(record: &EndpointRecord) -> Result<EndpointAddr> {
    let endpoint_id = super::endpoint_id(record)?;
    let mut address = EndpointAddr::new(endpoint_id);
    if let Some(relay_url) = &record.relay_url {
        address = address.with_relay_url(relay_url.parse().context("invalid scheduler relay URL")?);
    }
    for direct in &record.direct_addresses {
        address = address.with_ip_addr(direct.parse().context("invalid scheduler direct address")?);
    }
    Ok(address)
}

pub fn record_endpoint_id(record: &EndpointRecord) -> Result<EndpointId> {
    super::endpoint_id(record)
}
