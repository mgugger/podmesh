use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, EndpointId, endpoint::Connection};
use protocol::{
    DEFAULT_WORKLOAD_STREAM_TIMEOUT, EndpointRecord, ProxyDiscoveryRequest,
    ProxyEndpointDiscoveryResponse, SidecarRegistration, SidecarRegistrationAck, SidecarRoute,
    WORKLOAD_ALPN, WorkloadStreamKind, read_workload_frame, write_workload_frame,
};
use tokio_util::sync::CancellationToken;

use crate::SidecarConfig;

#[derive(Clone)]
pub struct ProxySession {
    pub connection: Connection,
    pub record: EndpointRecord,
    pub verified: bool,
}

pub async fn connect(
    endpoint: &Endpoint,
    config: &SidecarConfig,
    record: EndpointRecord,
    cancellation: &CancellationToken,
) -> Result<ProxySession> {
    let address = iroh_support::endpoint_addr(&record, now_secs()?)?;
    let expected = address.id;
    let connection = tokio::time::timeout(
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        endpoint.connect(address, WORKLOAD_ALPN),
    )
    .await
    .context("proxy connection timed out")?
    .context("connect proxy endpoint")?;
    ensure!(
        connection.remote_id() == expected,
        "connected proxy EndpointId does not match endpoint record"
    );
    let verified = authenticate(endpoint.id(), config, &connection, cancellation).await?;
    Ok(ProxySession {
        connection,
        record,
        verified,
    })
}

pub async fn register(
    local_endpoint: EndpointId,
    config: &SidecarConfig,
    session: &ProxySession,
    cancellation: &CancellationToken,
) -> Result<()> {
    ensure!(
        session.verified,
        "verified proxy grant is required for registration"
    );
    let owner = config
        .owner_public_key_b64
        .as_ref()
        .context("sidecar owner public key is required for registration")?;
    let endpoint_id = local_endpoint.to_string();
    let signed_data = format!("{}{}", config.manifest_id, endpoint_id);
    let (signing_public, signing_private) = crypto::ensure_keypair_on_disk()?;
    let signature = crypto::sign_data_with_key(&signing_private, signed_data.as_bytes())?;
    let registration = SidecarRegistration {
        manifest_id: config.manifest_id.clone(),
        routes: config
            .routes
            .iter()
            .map(|route| SidecarRoute {
                path_prefix: route.path_prefix.clone(),
                port: route.target_port,
            })
            .collect(),
        sidecar_peer_id: endpoint_id,
        owner_pubkey: owner.clone(),
        sig: crypto::b64_encode(&signature),
        sidecar_signing_pubkey: crypto::b64_encode(&signing_public),
    };
    let response = request_response(
        &session.connection,
        WorkloadStreamKind::Registration,
        &registration.to_bytes(),
        cancellation,
    )
    .await?;
    let acknowledgement = SidecarRegistrationAck::from_bytes(&response)
        .context("decode sidecar registration acknowledgement")?;
    ensure!(
        acknowledgement.ok,
        "sidecar registration rejected: {}",
        acknowledgement.message
    );
    log::info!(
        "sidecar registration acknowledged endpoint={} manifest={} routes={}",
        session.connection.remote_id().fmt_short(),
        config.manifest_id,
        registration.routes.len()
    );
    Ok(())
}

pub async fn discover(
    config: &SidecarConfig,
    session: &ProxySession,
    cancellation: &CancellationToken,
) -> Result<Vec<EndpointRecord>> {
    ensure!(
        session.verified,
        "verified proxy grant is required for discovery"
    );
    let owner = config
        .owner_public_key_b64
        .as_ref()
        .context("sidecar owner public key is required for proxy discovery")?;
    let request = ProxyDiscoveryRequest {
        owner_pubkey: owner.clone(),
        limit: protocol::proxy_endpoint_discovery::MAX_PROXY_ENDPOINTS as u16,
    };
    let response = request_response(
        &session.connection,
        WorkloadStreamKind::ProxyDiscovery,
        &request.to_bytes()?,
        cancellation,
    )
    .await?;
    Ok(ProxyEndpointDiscoveryResponse::from_bytes(&response, now_secs()?)?.endpoints)
}

async fn authenticate(
    local_endpoint: EndpointId,
    config: &SidecarConfig,
    connection: &Connection,
    cancellation: &CancellationToken,
) -> Result<bool> {
    let request = iroh_support::build_workload_handshake_request(
        local_endpoint,
        config.owner_public_key_b64.as_deref(),
    )?;
    let response = request_response(
        connection,
        WorkloadStreamKind::Handshake,
        &request,
        cancellation,
    )
    .await?;
    let verified = iroh_support::verify_workload_handshake(&response, connection.remote_id())?;
    let Some(owner) = config.owner_public_key_b64.as_ref() else {
        return Ok(false);
    };
    // The proxy proves it was authorized by this workload's owner. The endpoint
    // is taken from the authenticated transport rather than from the handshake,
    // so a grant leaked to a third party cannot be replayed by them.
    let encoded_grant = verified
        .handshake
        .proxy_grant_b64()
        .context("proxy handshake did not include an owner-signed grant")?;
    let grant = protocol::proxy_grant_from_b64(encoded_grant)?;
    let owner_public = crypto::b64_decode(owner).context("decode tenant owner key")?;
    protocol::verify_proxy_grant(
        &grant,
        &owner_public,
        owner,
        &connection.remote_id().to_string(),
        now_secs()?,
    )
    .context("verify proxy grant")?;
    Ok(true)
}

async fn request_response(
    connection: &Connection,
    kind: WorkloadStreamKind,
    payload: &[u8],
    cancellation: &CancellationToken,
) -> Result<Vec<u8>> {
    let (mut send, mut recv) =
        tokio::time::timeout(DEFAULT_WORKLOAD_STREAM_TIMEOUT, connection.open_bi())
            .await
            .context("workload stream open timed out")?
            .context("open workload stream")?;
    write_workload_frame(
        &mut send,
        kind,
        payload,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        cancellation,
    )
    .await?;
    send.finish().context("finish workload request")?;
    let (response_kind, response) =
        read_workload_frame(&mut recv, DEFAULT_WORKLOAD_STREAM_TIMEOUT, cancellation).await?;
    ensure!(response_kind == kind, "unexpected workload response kind");
    Ok(response)
}

fn now_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_secs())
}
