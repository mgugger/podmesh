use std::{collections::HashSet, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};
use clap::Args;
use iroh::{EndpointId, RelayMap};
use protocol::EndpointRecord;
use rustls_pki_types::CertificateDer;

pub const DEFAULT_MACHINE_BIND: &str = "0.0.0.0:0";
pub const DEFAULT_MAX_SCHEDULER_ATTACHMENTS: usize = 3;
pub const DEFAULT_RECONNECT_INITIAL_MS: u64 = 500;
pub const DEFAULT_RECONNECT_MAX_MS: u64 = 30_000;
pub const DEFAULT_MAX_SEEN_QUERIES: usize = 16_384;
pub const DEFAULT_OPERATION_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_MAX_CONCURRENT_UNI_STREAMS: u32 = 32;
pub const DEFAULT_MAX_CONCURRENT_BIDI_STREAMS: u32 = 8;
pub const DEFAULT_MAX_IDLE_SECS: u64 = 180;
pub const DEFAULT_STREAM_RECEIVE_WINDOW_BYTES: u32 = 64 * 1024;
pub const DEFAULT_CONNECTION_RECEIVE_WINDOW_BYTES: u32 = 1024 * 1024;
pub const MAX_SCHEDULER_ENDPOINTS: usize = 16;
pub const MAX_MACHINE_RELAYS: usize = 16;
pub const MAX_CONCURRENT_STREAMS: u32 = 1_024;

#[derive(Clone, Debug, Args)]
pub struct MachineConfig {
    #[arg(
        long = "machine-bind",
        env = "PODMESH_AGENT_MACHINE_BIND",
        default_value = DEFAULT_MACHINE_BIND
    )]
    pub bind_addr: SocketAddr,

    #[arg(
        long = "scheduler-endpoint",
        env = "PODMESH_AGENT_SCHEDULER_ENDPOINTS",
        value_delimiter = ','
    )]
    pub scheduler_endpoints: Vec<String>,

    /// Plain scheduler HTTP URLs to bootstrap `EndpointRecord`s from.
    ///
    /// Convenience only: each fetched record is signed by the scheduler and
    /// self-expiring, so this is equivalent to pasting the same records into
    /// `--scheduler-endpoint` by hand.
    #[arg(
        long = "scheduler-url",
        env = "PODMESH_AGENT_SCHEDULER_URLS",
        value_delimiter = ','
    )]
    pub scheduler_urls: Vec<String>,

    #[arg(
        long = "machine-relay-url",
        env = "PODMESH_AGENT_MACHINE_RELAY_URLS",
        value_delimiter = ','
    )]
    pub relay_urls: Vec<String>,

    #[arg(
        long = "machine-relay-ca-certificate",
        env = "PODMESH_AGENT_MACHINE_RELAY_CA_CERTIFICATES",
        value_delimiter = ','
    )]
    pub relay_ca_certificate_paths: Vec<PathBuf>,

    #[arg(
        long = "max-scheduler-attachments",
        env = "PODMESH_AGENT_MAX_SCHEDULER_ATTACHMENTS",
        default_value_t = DEFAULT_MAX_SCHEDULER_ATTACHMENTS
    )]
    pub max_scheduler_attachments: usize,

    #[arg(
        long = "attachment-reconnect-initial-ms",
        env = "PODMESH_AGENT_RECONNECT_INITIAL_MS",
        default_value_t = DEFAULT_RECONNECT_INITIAL_MS
    )]
    pub reconnect_initial_ms: u64,

    #[arg(
        long = "attachment-reconnect-max-ms",
        env = "PODMESH_AGENT_RECONNECT_MAX_MS",
        default_value_t = DEFAULT_RECONNECT_MAX_MS
    )]
    pub reconnect_max_ms: u64,

    #[arg(
        long = "max-seen-capacity-queries",
        env = "PODMESH_AGENT_MAX_SEEN_QUERIES",
        default_value_t = DEFAULT_MAX_SEEN_QUERIES
    )]
    pub max_seen_queries: usize,

    #[arg(
        long = "machine-operation-timeout-secs",
        env = "PODMESH_AGENT_MACHINE_TIMEOUT_SECS",
        default_value_t = DEFAULT_OPERATION_TIMEOUT_SECS
    )]
    pub operation_timeout_secs: u64,

    #[arg(long, default_value_t = DEFAULT_MAX_CONCURRENT_UNI_STREAMS)]
    pub max_concurrent_uni_streams: u32,

    #[arg(long, default_value_t = DEFAULT_MAX_CONCURRENT_BIDI_STREAMS)]
    pub max_concurrent_bidi_streams: u32,

    #[arg(long, default_value_t = DEFAULT_MAX_IDLE_SECS)]
    pub max_idle_secs: u64,

    #[arg(long, default_value_t = DEFAULT_STREAM_RECEIVE_WINDOW_BYTES)]
    pub stream_receive_window_bytes: u32,

    #[arg(long, default_value_t = DEFAULT_CONNECTION_RECEIVE_WINDOW_BYTES)]
    pub connection_receive_window_bytes: u32,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            bind_addr: DEFAULT_MACHINE_BIND
                .parse()
                .expect("valid default machine bind"),
            scheduler_endpoints: Vec::new(),
            scheduler_urls: Vec::new(),
            relay_urls: Vec::new(),
            relay_ca_certificate_paths: Vec::new(),
            max_scheduler_attachments: DEFAULT_MAX_SCHEDULER_ATTACHMENTS,
            reconnect_initial_ms: DEFAULT_RECONNECT_INITIAL_MS,
            reconnect_max_ms: DEFAULT_RECONNECT_MAX_MS,
            max_seen_queries: DEFAULT_MAX_SEEN_QUERIES,
            operation_timeout_secs: DEFAULT_OPERATION_TIMEOUT_SECS,
            max_concurrent_uni_streams: DEFAULT_MAX_CONCURRENT_UNI_STREAMS,
            max_concurrent_bidi_streams: DEFAULT_MAX_CONCURRENT_BIDI_STREAMS,
            max_idle_secs: DEFAULT_MAX_IDLE_SECS,
            stream_receive_window_bytes: DEFAULT_STREAM_RECEIVE_WINDOW_BYTES,
            connection_receive_window_bytes: DEFAULT_CONNECTION_RECEIVE_WINDOW_BYTES,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedMachineConfig {
    pub bind_addr: SocketAddr,
    pub scheduler_endpoints: Vec<EndpointRecord>,
    pub scheduler_ids: HashSet<EndpointId>,
    pub relay_urls: HashSet<String>,
    pub relay_ca_certificates: Vec<CertificateDer<'static>>,
    pub reconnect_initial: Duration,
    pub reconnect_max: Duration,
    pub max_seen_queries: usize,
    pub operation_timeout: Duration,
    pub max_concurrent_uni_streams: u32,
    pub max_concurrent_bidi_streams: u32,
    pub max_idle: Duration,
    pub stream_receive_window_bytes: u32,
    pub connection_receive_window_bytes: u32,
}

impl MachineConfig {
    pub fn validate(&self, now_secs: u64) -> Result<ValidatedMachineConfig> {
        ensure!(
            !self.scheduler_endpoints.is_empty()
                && self.scheduler_endpoints.len() <= MAX_SCHEDULER_ENDPOINTS,
            "agent requires between 1 and {MAX_SCHEDULER_ENDPOINTS} scheduler endpoints"
        );
        ensure!(
            self.scheduler_endpoints.len() <= self.max_scheduler_attachments,
            "scheduler endpoint count exceeds attachment limit"
        );
        let scheduler_endpoints = self
            .scheduler_endpoints
            .iter()
            .map(|encoded| {
                let bytes = crypto::b64_decode(encoded)?;
                EndpointRecord::from_bytes(&bytes, now_secs)
                    .context("invalid configured scheduler endpoint record")
            })
            .collect::<Result<Vec<_>>>()?;
        let scheduler_ids = scheduler_endpoints
            .iter()
            .map(|record| endpoint_id(record))
            .collect::<Result<HashSet<_>>>()?;
        ensure!(
            scheduler_ids.len() == scheduler_endpoints.len(),
            "duplicate scheduler EndpointId"
        );
        ensure!(
            scheduler_endpoints
                .iter()
                .all(|record| !record.direct_addresses.is_empty()),
            "each bootstrap scheduler requires a direct address"
        );

        ensure!(
            !self.relay_urls.is_empty() && self.relay_urls.len() <= MAX_MACHINE_RELAYS,
            "agent requires between 1 and {MAX_MACHINE_RELAYS} machine relay URLs"
        );
        let relay_map = RelayMap::try_from_iter(self.relay_urls.iter().map(String::as_str))
            .context("invalid agent machine relay URL")?;
        let relay_urls: HashSet<_> = relay_map
            .urls::<Vec<_>>()
            .into_iter()
            .map(|url| url.to_string())
            .collect();
        ensure!(
            relay_urls.len() == self.relay_urls.len(),
            "duplicate agent machine relay URL"
        );
        let relay_ca_certificates =
            iroh_support::load_ca_certificates(&self.relay_ca_certificate_paths)?;
        ensure!(
            self.max_scheduler_attachments > 0
                && self.max_scheduler_attachments <= MAX_SCHEDULER_ENDPOINTS,
            "agent scheduler attachment limit is invalid"
        );
        ensure!(
            self.reconnect_initial_ms > 0 && self.reconnect_initial_ms <= self.reconnect_max_ms,
            "agent reconnect backoff bounds are invalid"
        );
        ensure!(
            self.max_seen_queries > 0,
            "agent seen-query limit must be non-zero"
        );
        ensure!(
            self.operation_timeout_secs > 0,
            "agent machine operation timeout must be non-zero"
        );
        ensure!(
            (1..=MAX_CONCURRENT_STREAMS).contains(&self.max_concurrent_uni_streams)
                && (1..=MAX_CONCURRENT_STREAMS).contains(&self.max_concurrent_bidi_streams),
            "agent concurrent stream limits are invalid"
        );
        ensure!(
            self.max_idle_secs > 0,
            "agent idle timeout must be non-zero"
        );
        ensure!(
            self.stream_receive_window_bytes > 0
                && self.connection_receive_window_bytes >= self.stream_receive_window_bytes,
            "agent receive window bounds are invalid"
        );

        Ok(ValidatedMachineConfig {
            bind_addr: self.bind_addr,
            scheduler_endpoints,
            scheduler_ids,
            relay_urls,
            relay_ca_certificates,
            reconnect_initial: Duration::from_millis(self.reconnect_initial_ms),
            reconnect_max: Duration::from_millis(self.reconnect_max_ms),
            max_seen_queries: self.max_seen_queries,
            operation_timeout: Duration::from_secs(self.operation_timeout_secs),
            max_concurrent_uni_streams: self.max_concurrent_uni_streams,
            max_concurrent_bidi_streams: self.max_concurrent_bidi_streams,
            max_idle: Duration::from_secs(self.max_idle_secs),
            stream_receive_window_bytes: self.stream_receive_window_bytes,
            connection_receive_window_bytes: self.connection_receive_window_bytes,
        })
    }
}

pub fn endpoint_id(record: &EndpointRecord) -> Result<EndpointId> {
    let bytes: [u8; 32] = record
        .endpoint_id
        .as_slice()
        .try_into()
        .context("scheduler EndpointId length is invalid")?;
    EndpointId::from_bytes(&bytes).context("scheduler EndpointId is invalid")
}

#[cfg(test)]
mod config_tests;
