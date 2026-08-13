//! Helpers shared by the scheduler integration tests.
//!
//! Every test binary compiles this module separately, so a helper that only
//! some of them need is not dead code even though one binary sees it unused.
#![allow(dead_code, unused_imports)]

use std::{collections::HashSet, time::Duration};

use anyhow::{Context, Result};
use iroh::{
    Endpoint, EndpointId, SecretKey, address_lookup::memory::MemoryLookup, endpoint::presets,
};
use podmesh_scheduler::machine::{SchedulerIdentity, ValidatedMachineConfig};
use protocol::{CAPACITY_PROTOCOL_VERSION, CapacityQuery, ENDPOINT_RECORD_VERSION, EndpointRecord};

mod harness;

pub use harness::{EchoAgentControl, TestScheduler, attach, wait_for_attachment, wait_for_member};

pub const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A scheduler identity and an endpoint bound to the very same transport key.
///
/// They have to agree: a scheduler signs records for its own endpoint address,
/// and refuses to sign for an address that is not its transport identity. The
/// key directory must outlive both, so it is returned alongside.
pub async fn scheduler_endpoint(
    lookup: &MemoryLookup,
) -> Result<(Endpoint, SchedulerIdentity, tempfile::TempDir)> {
    let temp = tempfile::tempdir().context("create scheduler key directory")?;
    let identity = SchedulerIdentity::load(temp.path())?;
    let secret = iroh_support::load_or_initialize_iroh_secret(&temp.path().join("iroh"))
        .context("read back the scheduler transport key")?;
    let endpoint = endpoint(&secret, lookup).await?;
    Ok((endpoint, identity, temp))
}

pub async fn endpoint(secret: &SecretKey, lookup: &MemoryLookup) -> Result<Endpoint> {
    Endpoint::builder(presets::Minimal)
        .clear_relay_transports()
        .secret_key(secret.clone())
        .address_lookup(lookup.clone())
        .bind()
        .await
        .context("bind scheduler test endpoint")
}

pub fn config(
    scheduler_members: HashSet<EndpointId>,
    scheduler_bootstraps: Vec<EndpointId>,
) -> ValidatedMachineConfig {
    ValidatedMachineConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        relay_urls: vec!["https://relay.example.test".into()],
        relay_ca_certificates: Vec::new(),
        scheduler_members,
        scheduler_bootstraps,
        query_timeout: TEST_TIMEOUT,
        max_pending_queries: 16,
        max_seen_queries: 32,
        max_attached_agents: 16,
        max_offers_per_query: 8,
        max_agent_fanout: 8,
    }
}

pub fn signed_query(endpoint_id: &[u8; 32], now: u64) -> Result<CapacityQuery> {
    let (public, private) = crypto::generate_signing_keypair();
    let endpoint = EndpointRecord {
        version: ENDPOINT_RECORD_VERSION,
        endpoint_id: endpoint_id.to_vec(),
        relay_url: None,
        direct_addresses: vec!["127.0.0.1:4000".into()],
        signing_pubkey: String::new(),
        issued_at_secs: now,
        expires_at_secs: now + 15,
        signature: String::new(),
    }
    .sign(&public, &private, now)?;
    CapacityQuery {
        version: CAPACITY_PROTOCOL_VERSION,
        query_id: "query-1".into(),
        nonce: "nonce-1".into(),
        cpu_milli: 500,
        memory_bytes: 512 * 1024 * 1024,
        storage_bytes: 1024 * 1024 * 1024,
        required_capabilities: vec!["linux".into()],
        excluded_endpoint_ids: Vec::new(),
        reply_endpoint: endpoint,
        issued_at_secs: now,
        expires_at_secs: now + 5,
        signing_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(&public, &private, now)
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
