use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, RelayConfig};
use protocol::{
    AGENT_CAPACITY_ALPN, AgentAttachmentAck, CAPACITY_OFFER_ALPN, CapacityOffer, CapacityQuery,
    EndpointRecord, MAX_AGENT_ATTACHMENT_BYTES, MAX_CAPACITY_MESSAGE_BYTES, MachineRelayGrant,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::{SeenQueries, ValidatedMachineConfig, endpoint_addr};
use crate::AgentService;

const OFFER_ACK: &[u8] = b"ok";

pub async fn run_scheduler_attachment(
    endpoint: Endpoint,
    scheduler: EndpointRecord,
    config: ValidatedMachineConfig,
    service: AgentService,
    seen: Arc<Mutex<SeenQueries>>,
    cancellation: CancellationToken,
) -> Result<()> {
    let mut backoff = config.reconnect_initial;
    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        match run_session(
            &endpoint,
            &scheduler,
            &config,
            &service,
            &seen,
            &cancellation,
        )
        .await
        {
            Ok(()) if cancellation.is_cancelled() => return Ok(()),
            Ok(()) => backoff = config.reconnect_initial,
            Err(error) => {
                log::warn!(
                    "scheduler attachment {} failed: {error}",
                    super::record_endpoint_id(&scheduler)?.fmt_short()
                );
                tokio::select! {
                    _ = cancellation.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = backoff.saturating_mul(2).min(config.reconnect_max);
            }
        }
    }
}

async fn run_session(
    endpoint: &Endpoint,
    scheduler: &EndpointRecord,
    config: &ValidatedMachineConfig,
    service: &AgentService,
    seen: &Arc<Mutex<SeenQueries>>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let expected_scheduler = super::record_endpoint_id(scheduler)?;
    let connection = tokio::time::timeout(
        config.operation_timeout,
        endpoint.connect(endpoint_addr(scheduler)?, AGENT_CAPACITY_ALPN),
    )
    .await
    .context("scheduler attachment connect timed out")?
    .context("connect scheduler attachment")?;
    ensure!(
        connection.remote_id() == expected_scheduler,
        "scheduler attachment authenticated an unexpected EndpointId"
    );

    let now = now_secs();
    let hello = service.attachment_hello(&endpoint.addr(), now)?;
    let (mut send, mut recv) = tokio::time::timeout(config.operation_timeout, connection.open_bi())
        .await
        .context("attachment handshake stream timed out")?
        .context("open attachment handshake stream")?;
    send.write_all(&hello.to_bytes(now)?)
        .await
        .context("write attachment hello")?;
    send.finish().context("finish attachment hello")?;
    let ack_bytes = tokio::time::timeout(
        config.operation_timeout,
        recv.read_to_end(MAX_AGENT_ATTACHMENT_BYTES),
    )
    .await
    .context("attachment acknowledgement timed out")?
    .context("read attachment acknowledgement")?;
    let ack = AgentAttachmentAck::from_bytes(&ack_bytes, now_secs())?;
    install_relay_grants(endpoint, config, &ack).await?;
    log::info!(
        "agent attached to scheduler {}",
        expected_scheduler.fmt_short()
    );

    let refresh_wait = Duration::from_secs(ack.refresh_after_secs.saturating_sub(now_secs()));
    let refresh = tokio::time::sleep(refresh_wait);
    tokio::pin!(refresh);
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => {
                connection.close(0u8.into(), b"agent shutdown");
                return Ok(());
            }
            _ = &mut refresh => {
                connection.close(0u8.into(), b"refresh relay grant");
                return Ok(());
            }
            stream = connection.accept_uni() => {
                let mut stream = stream.context("accept scheduler capacity stream")?;
                let bytes = tokio::time::timeout(
                    config.operation_timeout,
                    stream.read_to_end(MAX_CAPACITY_MESSAGE_BYTES),
                )
                .await
                .context("capacity query read timed out")?
                .context("read capacity query")?;
                // A single rejected, expired, or undeliverable query must never
                // detach the agent from the scheduler: staying attached is what
                // keeps this agent eligible for every subsequent placement.
                if let Err(error) = process_query(endpoint, config, service, seen, &bytes).await {
                    log::warn!(
                        "capacity query from scheduler {} was not answered: {error}",
                        expected_scheduler.fmt_short()
                    );
                }
            }
        }
    }
}

async fn process_query(
    endpoint: &Endpoint,
    config: &ValidatedMachineConfig,
    service: &AgentService,
    seen: &Arc<Mutex<SeenQueries>>,
    bytes: &[u8],
) -> Result<()> {
    let now = now_secs();
    let query = CapacityQuery::from_bytes(bytes, now)?;
    ensure!(
        config.scheduler_endpoints.iter().any(|scheduler| {
            scheduler.endpoint_id == query.reply_endpoint.endpoint_id
                && scheduler.signing_pubkey == query.signing_pubkey
        }),
        "capacity query origin is not a configured scheduler"
    );
    if !seen.lock().await.insert(
        query.signing_pubkey.clone(),
        query.query_id.clone(),
        query.expires_at_secs,
        now,
    )? {
        return Ok(());
    }
    if let Some(offer) = service
        .capacity_offer(&query, &endpoint.addr(), now)
        .await?
    {
        send_offer(endpoint, config, &query.reply_endpoint, &offer).await?;
    }
    Ok(())
}

async fn send_offer(
    endpoint: &Endpoint,
    config: &ValidatedMachineConfig,
    destination: &EndpointRecord,
    offer: &CapacityOffer,
) -> Result<()> {
    let connection = tokio::time::timeout(
        config.operation_timeout,
        endpoint.connect(endpoint_addr(destination)?, CAPACITY_OFFER_ALPN),
    )
    .await
    .context("capacity offer connect timed out")?
    .context("connect capacity offer destination")?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("open capacity offer stream")?;
    send.write_all(&offer.to_bytes(now_secs())?)
        .await
        .context("write capacity offer")?;
    send.finish().context("finish capacity offer")?;
    let ack = tokio::time::timeout(config.operation_timeout, recv.read_to_end(OFFER_ACK.len()))
        .await
        .context("capacity offer acknowledgement timed out")?
        .context("read capacity offer acknowledgement")?;
    ensure!(ack == OFFER_ACK, "invalid capacity offer acknowledgement");
    connection.close(0u8.into(), b"capacity offer delivered");
    Ok(())
}

async fn install_relay_grants(
    endpoint: &Endpoint,
    config: &ValidatedMachineConfig,
    ack: &AgentAttachmentAck,
) -> Result<()> {
    ensure!(
        !ack.relay_grants.is_empty(),
        "scheduler attachment returned no machine relay grant"
    );
    let now = now_secs();
    let trusted_issuers = config
        .scheduler_endpoints
        .iter()
        .map(|scheduler| crypto::b64_decode(&scheduler.signing_pubkey))
        .collect::<Result<Vec<_>>>()?;
    let mut earliest_expiry = u64::MAX;
    for token in &ack.relay_grants {
        let grant = MachineRelayGrant::from_auth_token(token, now)?;
        ensure!(
            config.relay_urls.contains(&grant.relay_audience),
            "scheduler issued a grant for an unconfigured relay"
        );
        grant.verify(
            &trusted_issuers,
            endpoint.id().as_bytes(),
            &grant.relay_audience,
            now,
        )?;
        earliest_expiry = earliest_expiry.min(grant.expires_at_secs);
        let relay_url = iroh::RelayMap::try_from_iter([grant.relay_audience.as_str()])
            .context("invalid relay grant audience")?
            .urls::<Vec<_>>()
            .into_iter()
            .next()
            .context("relay grant audience is missing")?;
        let relay = RelayConfig::from(relay_url).with_auth_token(token.clone());
        endpoint
            .insert_relay(relay.url.clone(), Arc::new(relay))
            .await;
    }
    ensure!(
        ack.refresh_after_secs < earliest_expiry,
        "relay grant refresh deadline is not before expiry"
    );
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
