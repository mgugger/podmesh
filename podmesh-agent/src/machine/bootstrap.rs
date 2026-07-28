//! HTTP bootstrap of scheduler reachability.
//!
//! An agent cannot dial a scheduler over Iroh until it knows that scheduler's
//! `EndpointRecord`, which makes the record itself a chicken-and-egg problem
//! for a fresh deployment. Rather than forcing an operator to copy base64 blobs
//! around, the agent may be pointed at plain scheduler HTTP URLs and fetch the
//! records itself.
//!
//! This does not widen the trust model. Every fetched record is signed by the
//! scheduler's own key and carries its own expiry, so a hostile HTTP hop can
//! withhold or stall a record but cannot forge one that the machine plane will
//! accept.

use std::time::Duration;

use anyhow::{Context, Result, ensure};

/// Upper bound on how many scheduler URLs an agent may bootstrap from. Matches
/// the scheduler endpoint limit so a URL list can never smuggle in more
/// attachments than the endpoint list would allow.
pub const MAX_BOOTSTRAP_URLS: usize = super::config::MAX_SCHEDULER_ENDPOINTS;

/// Total time allowed for a single scheduler bootstrap request. A scheduler
/// that cannot answer this quickly is treated as unavailable.
const BOOTSTRAP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Refuses to buffer an oversized bootstrap response body.
const MAX_BOOTSTRAP_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(serde::Deserialize)]
struct EndpointRecordResponse {
    endpoint_record_b64: String,
}

/// Fetches one signed `EndpointRecord` per scheduler URL, base64 encoded.
///
/// Every URL must answer. A partially bootstrapped agent would silently run
/// with fewer scheduler attachments than the operator asked for, so a single
/// unreachable scheduler is a hard error the supervisor can retry.
pub async fn resolve_scheduler_urls(urls: &[String]) -> Result<Vec<String>> {
    ensure!(
        urls.len() <= MAX_BOOTSTRAP_URLS,
        "at most {MAX_BOOTSTRAP_URLS} scheduler bootstrap URLs are supported"
    );
    let client = reqwest::Client::builder()
        .timeout(BOOTSTRAP_REQUEST_TIMEOUT)
        .build()
        .context("build scheduler bootstrap HTTP client")?;
    let mut records = Vec::with_capacity(urls.len());
    for url in urls {
        let base = url.trim().trim_end_matches('/');
        ensure!(!base.is_empty(), "empty scheduler bootstrap URL");
        let response = client
            .get(format!("{base}/api/v1/endpoint_record"))
            .send()
            .await
            .with_context(|| format!("GET {base}/api/v1/endpoint_record failed"))?
            .error_for_status()
            .with_context(|| format!("scheduler {base} refused to publish its endpoint record"))?;
        let body = response
            .bytes()
            .await
            .with_context(|| format!("read endpoint record body from {base}"))?;
        ensure!(
            body.len() <= MAX_BOOTSTRAP_RESPONSE_BYTES,
            "scheduler {base} returned an oversized endpoint record response"
        );
        let parsed: EndpointRecordResponse = serde_json::from_slice(&body)
            .with_context(|| format!("decode endpoint record response from {base}"))?;
        log::info!("bootstrapped scheduler endpoint record from {base}");
        records.push(parsed.endpoint_record_b64);
    }
    Ok(records)
}
