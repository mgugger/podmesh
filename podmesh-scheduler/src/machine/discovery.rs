//! Background discovery of peer schedulers over plain HTTP.
//!
//! Schedulers cannot know each other's EndpointIds before they first boot, and
//! a set of schedulers that all block on each other at startup would deadlock.
//! This task instead polls the peers' HTTP APIs until they answer, admitting
//! each one into the gossip allowlist and the machine relay's issuer trust as
//! it appears, then dialing it into the gossip mesh.
//!
//! HTTP is used only for reachability, never for authority: every record is
//! signed by the peer's own key and self-expiring, so an intermediary can stall
//! or withhold discovery but cannot inject a scheduler into the mesh.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use iroh::{EndpointAddr, EndpointId, address_lookup::memory::MemoryLookup};
use tokio_util::sync::CancellationToken;

use crate::machine::{IssuerRegistry, MemberRegistry, PeerJoiner};

/// Upper bound on configured peer URLs, matched to the member allowlist bound.
pub const MAX_PEER_URLS: usize = 16;

/// Total time allowed for a single peer discovery request.
const DISCOVERY_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Delay between discovery sweeps. Short enough that a mesh started all at
/// once converges quickly, long enough not to hammer a peer that is down.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(10);

/// Refuses to buffer an oversized discovery response body.
const MAX_DISCOVERY_RESPONSE_BYTES: usize = 16 * 1024;

/// Ed25519 public keys are 32 bytes; anything else is not a signing key.
const SIGNING_KEY_BYTES: usize = 32;

#[derive(serde::Deserialize)]
struct EndpointRecordResponse {
    endpoint_record_b64: String,
    signing_pubkey_b64: String,
}

/// Polls `peer_urls` until cancelled, converging membership and relay trust.
pub async fn run_peer_discovery(
    peer_urls: Vec<String>,
    local_endpoint: EndpointId,
    members: MemberRegistry,
    issuers: IssuerRegistry,
    joiner: PeerJoiner,
    lookup: MemoryLookup,
    cancellation: CancellationToken,
) -> Result<()> {
    ensure!(
        peer_urls.len() <= MAX_PEER_URLS,
        "at most {MAX_PEER_URLS} scheduler peer URLs are supported"
    );
    let client = reqwest::Client::builder()
        .timeout(DISCOVERY_REQUEST_TIMEOUT)
        .build()
        .context("build scheduler peer discovery HTTP client")?;
    let mut ticker = tokio::time::interval(DISCOVERY_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            _ = ticker.tick() => {
                let mut discovered = Vec::new();
                for url in &peer_urls {
                    match discover_peer(&client, url).await {
                        Ok((address, signing_key)) => {
                            let endpoint_id = address.id;
                            if endpoint_id == local_endpoint {
                                continue;
                            }
                            // Refreshed on every sweep so a peer that restarts
                            // on a new address stays dialable.
                            lookup.set_endpoint_info(address);
                            if issuers.insert(signing_key) {
                                log::info!("machine relay now trusts scheduler {url}");
                            }
                            if members.insert(endpoint_id) {
                                log::info!(
                                    "admitted scheduler {} ({url}) into the gossip mesh",
                                    endpoint_id.fmt_short()
                                );
                                discovered.push(endpoint_id);
                            }
                        }
                        // A peer that is down is the normal case during a
                        // rolling start, so this stays at debug level.
                        Err(error) => log::debug!("scheduler peer {url} not reachable yet: {error:#}"),
                    }
                }
                if !discovered.is_empty() {
                    // Failing to dial is not fatal: the peers stay in the
                    // allowlist, so the next sweep or their own dial completes
                    // the mesh.
                    if let Err(error) = joiner.join_peers(discovered).await {
                        log::warn!("dialing discovered scheduler peers failed: {error:#}");
                    }
                }
            }
        }
    }
}

async fn discover_peer(client: &reqwest::Client, url: &str) -> Result<(EndpointAddr, Vec<u8>)> {
    let base = url.trim().trim_end_matches('/');
    ensure!(!base.is_empty(), "empty scheduler peer URL");
    let response = client
        .get(format!("{base}/api/v1/endpoint_record"))
        .send()
        .await
        .context("request peer endpoint record")?
        .error_for_status()
        .context("peer refused to publish its endpoint record")?;
    let body = response
        .bytes()
        .await
        .context("read peer endpoint record body")?;
    ensure!(
        body.len() <= MAX_DISCOVERY_RESPONSE_BYTES,
        "peer returned an oversized endpoint record response"
    );
    let parsed: EndpointRecordResponse =
        serde_json::from_slice(&body).context("decode peer endpoint record response")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before the unix epoch")?
        .as_secs();
    // Verifying the record is what makes HTTP discovery safe: an intermediary
    // that rewrites the response cannot produce a valid signature.
    let record = protocol::EndpointRecord::from_bytes(
        &crypto::b64_decode(&parsed.endpoint_record_b64)?,
        now,
    )
    .context("verify peer endpoint record")?;
    // The relay and direct addresses the record carries are what makes the peer
    // dialable; discarding them leaves gossip with an EndpointId it cannot use.
    let address = iroh_support::endpoint_addr(&record, now).context("peer address is invalid")?;
    let signing_key =
        crypto::b64_decode(&parsed.signing_pubkey_b64).context("decode peer signing public key")?;
    ensure!(
        signing_key.len() == SIGNING_KEY_BYTES,
        "peer signing public key must contain {SIGNING_KEY_BYTES} bytes"
    );
    ensure!(
        crypto::b64_encode(&signing_key) == record.signing_pubkey,
        "peer signing key does not match the key that signed its endpoint record"
    );
    Ok((address, signing_key))
}
