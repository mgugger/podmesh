//! Background discovery of peer schedulers over plain HTTP.
//!
//! Schedulers cannot know each other's EndpointIds before they first boot, and
//! a set of schedulers that all block on each other at startup would deadlock.
//! This task instead polls the peers' HTTP APIs until they answer, admitting
//! each one into the gossip allowlist and the machine relay's issuer trust as
//! it appears, then dialing it into the gossip mesh.
//!
//! HTTP is used only for reachability, never for authority. A signature alone
//! settles nothing here, because a peer record is self-signed: whoever answers
//! the URL picks the key that validates it. Each peer URL is therefore bound to
//! one identity by [`PeerPins`], so an intermediary can stall or withhold
//! discovery but cannot substitute a scheduler of its own.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use iroh::{EndpointAddr, EndpointId, address_lookup::memory::MemoryLookup};
use tokio_util::sync::CancellationToken;

use crate::machine::{
    IssuerRegistry, MemberRegistry, PeerJoiner,
    peer_pins::{PeerPin, PeerPins},
};
use std::sync::Arc;

/// Upper bound on configured peer URLs, matched to the member allowlist bound.
pub const MAX_PEER_URLS: usize = 16;

/// Total time allowed for a single peer discovery request.
const DISCOVERY_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Delay between discovery sweeps. Short enough that a mesh started all at
/// once converges quickly, long enough not to hammer a peer that is down.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(10);

/// How often a scheduler announces itself on the gossip mesh.
///
/// Announcements are what let membership grow past the peers configured on each
/// node: a scheduler admitted by one operator-configured peer becomes reachable
/// to the whole mesh without anybody editing configuration elsewhere. Repeating
/// them refreshes addresses and reaches schedulers that joined later.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

/// Lifetime stamped on an announcement, comfortably longer than the interval so
/// a missed round does not expire a peer.
const ANNOUNCE_LIFETIME_SECS: u64 = 300;

/// Refuses to buffer an oversized discovery response body.
const MAX_DISCOVERY_RESPONSE_BYTES: usize = 16 * 1024;

/// Ed25519 public keys are 32 bytes; anything else is not a signing key.
const SIGNING_KEY_BYTES: usize = 32;

#[derive(serde::Deserialize)]
struct EndpointRecordResponse {
    endpoint_record_b64: String,
    signing_pubkey_b64: String,
}

/// Everything peer discovery converges into.
pub struct PeerDiscovery {
    pub peer_urls: Vec<String>,
    pub local_endpoint: EndpointId,
    pub members: MemberRegistry,
    pub issuers: IssuerRegistry,
    pub joiner: PeerJoiner,
    pub lookup: MemoryLookup,
    pub pins: Arc<PeerPins>,
    /// Publishes this scheduler's own announcements.
    pub publisher: crate::machine::GossipPublisher,
    /// Identity used to sign them.
    pub identity: crate::machine::SchedulerIdentity,
    /// Address peers should use to reach this scheduler.
    pub endpoint: iroh::Endpoint,
}

/// Polls `peer_urls` until cancelled, converging membership and relay trust.
pub async fn run_peer_discovery(
    discovery: PeerDiscovery,
    cancellation: CancellationToken,
) -> Result<()> {
    let PeerDiscovery {
        peer_urls,
        local_endpoint,
        members,
        issuers,
        joiner,
        lookup,
        pins,
        publisher,
        identity,
        endpoint,
    } = discovery;
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
    let mut announce = tokio::time::interval(ANNOUNCE_INTERVAL);
    announce.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            _ = announce.tick() => {
                match announcement(&identity, &endpoint) {
                    Ok(record) => {
                        if let Err(error) = publisher.publish_announcement(record).await {
                            log::debug!("announcing this scheduler failed: {error:#}");
                        }
                    }
                    Err(error) => log::warn!("building a scheduler announcement failed: {error:#}"),
                }
            }
            _ = ticker.tick() => {
                let mut discovered = Vec::new();
                for url in &peer_urls {
                    match discover_peer(&client, url, &pins).await {
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

async fn discover_peer(
    client: &reqwest::Client,
    url: &str,
    pins: &PeerPins,
) -> Result<(EndpointAddr, Vec<u8>)> {
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
    // Everything above only proves the response is internally consistent. The
    // pin is what ties this URL to one identity across restarts.
    pins.accept(url, &PeerPin::new(address.id, &signing_key))
        .context("peer identity does not match its pinned identity")?;
    Ok((address, signing_key))
}

/// This scheduler's own signed record, for announcing to the mesh.
fn announcement(
    identity: &crate::machine::SchedulerIdentity,
    endpoint: &iroh::Endpoint,
) -> Result<protocol::EndpointRecord> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before the unix epoch")?
        .as_secs();
    identity.endpoint_record(&endpoint.addr(), now, now + ANNOUNCE_LIFETIME_SECS)
}
