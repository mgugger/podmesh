use std::{
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, EndpointId, endpoint::Connection};
use protocol::{
    AcceptedWorkloadPayload, DEFAULT_WORKLOAD_STREAM_TIMEOUT, EndpointRecord,
    ProxyDiscoveryRequest, ProxyEndpointDiscoveryResponse, SidecarRegistration,
    SidecarRegistrationAck, SidecarRoute, WORKLOAD_ALPN, WorkloadEnvelopeParts, WorkloadPayload,
    accept_workload_payload, read_workload_frame, seal_workload_payload, write_workload_frame,
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::SidecarConfig;

#[derive(Clone)]
pub struct ProxySession {
    pub connection: Connection,
    pub record: EndpointRecord,
    pub verified: bool,
    pub proxy_grant: Vec<u8>,
    pub owner_pubkey: String,
    pub owner_public: Vec<u8>,
    pub identity: iroh_support::NodeIdentity,
    pub replay_registry: Arc<protocol::PeerReplayRegistry>,
    pub stream_slots: Arc<Semaphore>,
    pub metrics: podmesh_metrics::Metrics,
}

pub async fn connect(
    endpoint: &Endpoint,
    identity: &iroh_support::NodeIdentity,
    config: &SidecarConfig,
    record: EndpointRecord,
    replay_registry: Arc<protocol::PeerReplayRegistry>,
    metrics: podmesh_metrics::Metrics,
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
    let handshake_timer =
        metrics.operation_started(podmesh_metrics::OperationName::WorkloadHandshake);
    let proxy_grant = authenticate(
        endpoint.id(),
        identity,
        config,
        &connection,
        &replay_registry,
        cancellation,
    )
    .await;
    super::finish_operation(handshake_timer, &proxy_grant, &metrics);
    let proxy_grant = proxy_grant?;
    Ok(ProxySession {
        connection,
        record,
        verified: true,
        proxy_grant,
        owner_pubkey: config
            .owner_public_key_b64
            .clone()
            .context("sidecar owner key is missing")?,
        owner_public: crypto::b64_decode(
            config
                .owner_public_key_b64
                .as_deref()
                .context("sidecar owner key is missing")?,
        )?,
        identity: identity.clone(),
        replay_registry,
        metrics,
        stream_slots: Arc::new(Semaphore::new(
            protocol::MAX_WORKLOAD_STREAMS_PER_CONNECTION,
        )),
    })
}

pub async fn register(
    local_endpoint: EndpointId,
    config: &SidecarConfig,
    session: &ProxySession,
    cancellation: &CancellationToken,
) -> Result<()> {
    let timer = session
        .metrics
        .operation_started(podmesh_metrics::OperationName::Registration);
    let result = register_inner(local_endpoint, config, session, cancellation).await;
    super::finish_operation(timer, &result, &session.metrics);
    result
}

async fn register_inner(
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
    // The routing key is derived from the owner key rather than chosen, so this
    // registration can only ever claim routes for its own tenant. There is no
    // sidecar-held signature: the sidecar's key is self-generated and would
    // prove nothing, while the transport already authenticates the endpoint.
    let registration = SidecarRegistration::new(
        &crypto::b64_decode(owner).context("decode tenant owner key")?,
        &config.workload_name,
        config.replica_index,
        config.replica_count,
        config
            .routes
            .iter()
            .map(|route| SidecarRoute {
                host: route.host.clone(),
                path_prefix: route.path_prefix.clone(),
                port: route.target_port,
            })
            .collect(),
        &local_endpoint.to_string(),
    );
    let accepted = request_response::<_, SidecarRegistrationAck>(
        local_endpoint,
        &session.connection,
        &session.identity,
        &session.replay_registry,
        Some(&session.stream_slots),
        &registration,
        cancellation,
    )
    .await?;
    let acknowledgement = accepted.payload;
    ensure!(
        acknowledgement.ok,
        "sidecar registration rejected: {}",
        acknowledgement.message
    );
    log::info!(
        "sidecar registration acknowledged endpoint={} manifest={} routes={}",
        session.connection.remote_id().fmt_short(),
        registration.manifest_id,
        registration.routes.len()
    );
    Ok(())
}

pub async fn discover(
    config: &SidecarConfig,
    session: &ProxySession,
    cancellation: &CancellationToken,
) -> Result<Vec<EndpointRecord>> {
    let timer = session
        .metrics
        .operation_started(podmesh_metrics::OperationName::Discovery);
    let result = discover_inner(config, session, cancellation).await;
    super::finish_operation(timer, &result, &session.metrics);
    result
}

async fn discover_inner(
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
    let accepted = request_response::<_, ProxyEndpointDiscoveryResponse>(
        session.identity.endpoint_id(),
        &session.connection,
        &session.identity,
        &session.replay_registry,
        Some(&session.stream_slots),
        &request,
        cancellation,
    )
    .await?;
    Ok(accepted.payload.endpoints)
}

async fn authenticate(
    local_endpoint: EndpointId,
    identity: &iroh_support::NodeIdentity,
    config: &SidecarConfig,
    connection: &Connection,
    replay_registry: &protocol::PeerReplayRegistry,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>> {
    // The owner key alone is public, so it is sent together with the credential
    // that proves this pod was deployed by that owner for this routing key.
    let owner = config
        .owner_public_key_b64
        .as_deref()
        .context("sidecar was injected without a tenant owner key")?;
    let credential = config
        .workload_credential_b64
        .as_deref()
        .context("sidecar was injected without a workload credential")?;
    let request =
        protocol::machine::WorkloadHandshakeRequest::new(owner, &config.manifest_id, credential);
    let accepted = request_response::<_, protocol::machine::WorkloadHandshakeResponse>(
        local_endpoint,
        connection,
        identity,
        replay_registry,
        None,
        &request,
        cancellation,
    )
    .await?;
    // Without an owner key there is nothing to check the proxy's grant against,
    // so the session cannot be trusted with tenant traffic at all. This is a
    // hard failure rather than an unverified session, because callers would
    // otherwise have to remember to gate on `verified` everywhere.
    // The proxy proves it was authorized by this workload's owner. The endpoint
    // is taken from the authenticated transport rather than from the handshake,
    // so a grant leaked to a third party cannot be replayed by them.
    let encoded_grant = (!accepted.payload.proxy_grant_b64.is_empty())
        .then_some(accepted.payload.proxy_grant_b64.as_str())
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
    Ok(grant)
}

fn now_millis() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_millis()
        .try_into()
        .context("system time exceeds u64 milliseconds")
}

async fn request_response<Req, Res>(
    local_endpoint: EndpointId,
    connection: &Connection,
    identity: &iroh_support::NodeIdentity,
    replay_registry: &protocol::PeerReplayRegistry,
    stream_slots: Option<&Arc<Semaphore>>,
    request: &Req,
    cancellation: &CancellationToken,
) -> Result<AcceptedWorkloadPayload<Res>>
where
    Req: WorkloadPayload,
    Res: WorkloadPayload,
{
    let _connection_permit = if let Some(slots) = stream_slots {
        Some(
            slots
                .clone()
                .try_acquire_owned()
                .context("proxy connection stream limit reached")?,
        )
    } else {
        None
    };
    ensure!(
        Req::TYPE.response() == Some(Res::TYPE),
        "workload request and response types are not paired"
    );
    let envelope = seal_workload_payload(
        WorkloadEnvelopeParts {
            nonce: &crypto::generate_secure_nonce(),
            now_millis: now_millis()?,
            sender_id: &local_endpoint.to_string(),
            recipient_id: &connection.remote_id().to_string(),
            sender_signing_public: identity.signing_public(),
            sender_signing_private: identity.signing_private(),
            sender_kem_public: Some(identity.kem_public()),
        },
        request,
    )?;
    let (mut send, mut recv) =
        tokio::time::timeout(DEFAULT_WORKLOAD_STREAM_TIMEOUT, connection.open_bi())
            .await
            .context("workload stream open timed out")?
            .context("open workload stream")?;
    write_workload_frame(
        &mut send,
        Req::TYPE.frame_kind(),
        &envelope,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        cancellation,
    )
    .await?;
    send.finish().context("finish workload request")?;
    let (response_kind, response) =
        read_workload_frame(&mut recv, DEFAULT_WORKLOAD_STREAM_TIMEOUT, cancellation).await?;
    ensure!(
        response_kind == Res::TYPE.frame_kind(),
        "unexpected workload response kind"
    );
    accept_workload_payload::<Res>(
        &response,
        &local_endpoint.to_string(),
        &connection.remote_id().to_string(),
        now_millis()?,
        replay_registry,
        Instant::now(),
    )
}

fn now_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_secs())
}
