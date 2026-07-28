use std::{collections::HashSet, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};
use clap::Args;
use iroh::{EndpointId, RelayMap};
use protocol::capacity::MAX_CAPACITY_QUERY_LIFETIME_SECS;
use rustls_pki_types::CertificateDer;

pub const DEFAULT_MACHINE_BIND: &str = "0.0.0.0:0";
pub const DEFAULT_QUERY_TIMEOUT_SECS: u64 = 5;
pub const DEFAULT_MAX_PENDING_QUERIES: usize = 1_024;
pub const DEFAULT_MAX_SEEN_QUERIES: usize = 16_384;
pub const DEFAULT_MAX_ATTACHED_AGENTS: usize = 10_000;
pub const DEFAULT_MAX_OFFERS_PER_QUERY: usize = 256;
pub const DEFAULT_MAX_AGENT_FANOUT: usize = 1_024;
pub const MAX_MACHINE_RELAYS: usize = 16;
pub const MAX_SCHEDULER_MEMBERS: usize = 256;
pub const MAX_SCHEDULER_BOOTSTRAPS: usize = 32;

#[derive(Clone, Debug, Args)]
pub struct MachineConfig {
    #[arg(
        long = "machine-key-dir",
        env = "PODMESH_SCHEDULER_MACHINE_KEY_DIR",
        default_value = "/etc/podmesh/scheduler"
    )]
    pub key_dir: PathBuf,

    #[arg(
        long = "machine-bind",
        env = "PODMESH_SCHEDULER_MACHINE_BIND",
        default_value = DEFAULT_MACHINE_BIND
    )]
    pub bind_addr: SocketAddr,

    #[arg(
        long = "machine-relay-url",
        env = "PODMESH_SCHEDULER_MACHINE_RELAY_URLS",
        value_delimiter = ','
    )]
    pub relay_urls: Vec<String>,

    #[arg(
        long = "machine-relay-ca-certificate",
        env = "PODMESH_SCHEDULER_MACHINE_RELAY_CA_CERTIFICATES",
        value_delimiter = ','
    )]
    pub relay_ca_certificate_paths: Vec<PathBuf>,

    #[arg(
        long = "scheduler-member",
        env = "PODMESH_SCHEDULER_MEMBERS",
        value_delimiter = ','
    )]
    pub scheduler_members: Vec<String>,

    #[arg(
        long = "scheduler-bootstrap",
        env = "PODMESH_SCHEDULER_BOOTSTRAPS",
        value_delimiter = ','
    )]
    pub scheduler_bootstraps: Vec<String>,

    /// Plain HTTP URLs of peer schedulers to discover continuously.
    ///
    /// Every scheduler in a mesh can list every peer, including itself. Peers
    /// are admitted as they come up, so a set of schedulers started together
    /// converges rather than deadlocking on each other. The scheduler's own
    /// URL is ignored.
    #[arg(
        long = "scheduler-peer-url",
        env = "PODMESH_SCHEDULER_PEER_URLS",
        value_delimiter = ','
    )]
    pub scheduler_peer_urls: Vec<String>,

    #[arg(
        long = "capacity-query-timeout-secs",
        env = "PODMESH_SCHEDULER_QUERY_TIMEOUT_SECS",
        default_value_t = DEFAULT_QUERY_TIMEOUT_SECS
    )]
    pub query_timeout_secs: u64,

    #[arg(long, default_value_t = DEFAULT_MAX_PENDING_QUERIES)]
    pub max_pending_queries: usize,

    #[arg(long, default_value_t = DEFAULT_MAX_SEEN_QUERIES)]
    pub max_seen_queries: usize,

    #[arg(long, default_value_t = DEFAULT_MAX_ATTACHED_AGENTS)]
    pub max_attached_agents: usize,

    #[arg(long, default_value_t = DEFAULT_MAX_OFFERS_PER_QUERY)]
    pub max_offers_per_query: usize,

    #[arg(long, default_value_t = DEFAULT_MAX_AGENT_FANOUT)]
    pub max_agent_fanout: usize,
}

#[derive(Clone, Debug)]
pub struct ValidatedMachineConfig {
    pub bind_addr: SocketAddr,
    pub relay_urls: Vec<String>,
    pub relay_ca_certificates: Vec<CertificateDer<'static>>,
    pub scheduler_members: HashSet<EndpointId>,
    pub scheduler_bootstraps: Vec<EndpointId>,
    pub query_timeout: Duration,
    pub max_pending_queries: usize,
    pub max_seen_queries: usize,
    pub max_attached_agents: usize,
    pub max_offers_per_query: usize,
    pub max_agent_fanout: usize,
}

impl MachineConfig {
    pub fn validate(&self, own_endpoint_id: EndpointId) -> Result<ValidatedMachineConfig> {
        ensure!(
            !self.relay_urls.is_empty() && self.relay_urls.len() <= MAX_MACHINE_RELAYS,
            "machine relay map requires between 1 and {MAX_MACHINE_RELAYS} URLs"
        );
        let relay_urls: HashSet<_> = self.relay_urls.iter().collect();
        ensure!(
            relay_urls.len() == self.relay_urls.len(),
            "duplicate machine relay URL"
        );
        RelayMap::try_from_iter(self.relay_urls.iter().map(String::as_str))
            .context("invalid machine relay URL")?;
        let relay_ca_certificates =
            iroh_support::load_ca_certificates(&self.relay_ca_certificate_paths)?;

        ensure!(
            self.scheduler_members.len() <= MAX_SCHEDULER_MEMBERS,
            "too many scheduler members"
        );
        let mut scheduler_members = self
            .scheduler_members
            .iter()
            .map(|value| {
                value
                    .parse::<EndpointId>()
                    .context("invalid scheduler member EndpointId")
            })
            .collect::<Result<HashSet<_>>>()?;
        ensure!(
            scheduler_members.len() == self.scheduler_members.len(),
            "duplicate scheduler member EndpointId"
        );
        scheduler_members.insert(own_endpoint_id);

        ensure!(
            self.scheduler_bootstraps.len() <= MAX_SCHEDULER_BOOTSTRAPS,
            "too many scheduler bootstrap peers"
        );
        let scheduler_bootstraps = self
            .scheduler_bootstraps
            .iter()
            .map(|value| {
                value
                    .parse::<EndpointId>()
                    .context("invalid scheduler bootstrap EndpointId")
            })
            .collect::<Result<Vec<_>>>()?;
        let unique_bootstraps: HashSet<_> = scheduler_bootstraps.iter().copied().collect();
        ensure!(
            unique_bootstraps.len() == scheduler_bootstraps.len(),
            "duplicate scheduler bootstrap EndpointId"
        );
        ensure!(
            scheduler_bootstraps
                .iter()
                .all(|endpoint_id| scheduler_members.contains(endpoint_id)),
            "every scheduler bootstrap must be an authorized scheduler member"
        );
        ensure!(
            (1..=MAX_CAPACITY_QUERY_LIFETIME_SECS).contains(&self.query_timeout_secs),
            "capacity query timeout is outside protocol bounds"
        );
        for (name, value) in [
            ("max pending queries", self.max_pending_queries),
            ("max seen queries", self.max_seen_queries),
            ("max attached agents", self.max_attached_agents),
            ("max offers per query", self.max_offers_per_query),
            ("max agent fanout", self.max_agent_fanout),
        ] {
            ensure!(value > 0, "{name} must be non-zero");
        }
        ensure!(
            self.max_agent_fanout <= self.max_attached_agents,
            "agent fanout cannot exceed attached-agent limit"
        );

        Ok(ValidatedMachineConfig {
            bind_addr: self.bind_addr,
            relay_urls: self.relay_urls.clone(),
            relay_ca_certificates,
            scheduler_members,
            scheduler_bootstraps,
            query_timeout: Duration::from_secs(self.query_timeout_secs),
            max_pending_queries: self.max_pending_queries,
            max_seen_queries: self.max_seen_queries,
            max_attached_agents: self.max_attached_agents,
            max_offers_per_query: self.max_offers_per_query,
            max_agent_fanout: self.max_agent_fanout,
        })
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    fn config(member: EndpointId) -> MachineConfig {
        MachineConfig {
            scheduler_peer_urls: Vec::new(),
            key_dir: "/tmp/podmesh-scheduler-test".into(),
            bind_addr: DEFAULT_MACHINE_BIND.parse().unwrap(),
            relay_urls: vec!["https://relay.example.test".into()],
            relay_ca_certificate_paths: Vec::new(),
            scheduler_members: vec![member.to_string()],
            scheduler_bootstraps: vec![member.to_string()],
            query_timeout_secs: DEFAULT_QUERY_TIMEOUT_SECS,
            max_pending_queries: DEFAULT_MAX_PENDING_QUERIES,
            max_seen_queries: DEFAULT_MAX_SEEN_QUERIES,
            max_attached_agents: DEFAULT_MAX_ATTACHED_AGENTS,
            max_offers_per_query: DEFAULT_MAX_OFFERS_PER_QUERY,
            max_agent_fanout: DEFAULT_MAX_AGENT_FANOUT,
        }
    }

    #[test]
    fn valid_custom_relay_membership_and_bootstrap_configuration_passes() {
        let own = SecretKey::generate().public();
        let peer = SecretKey::generate().public();
        let validated = config(peer).validate(own).unwrap();
        assert!(validated.scheduler_members.contains(&own));
        assert!(validated.scheduler_members.contains(&peer));
        assert_eq!(validated.scheduler_bootstraps, vec![peer]);
    }

    #[test]
    fn missing_relays_and_unauthorized_bootstraps_fail_closed() {
        let own = SecretKey::generate().public();
        let member = SecretKey::generate().public();
        let mut value = config(member);
        value.relay_urls.clear();
        assert!(value.validate(own).is_err());

        let mut value = config(member);
        value.scheduler_bootstraps = vec![SecretKey::generate().public().to_string()];
        assert!(value.validate(own).is_err());
    }

    #[test]
    fn query_and_attachment_limits_are_strict() {
        let own = SecretKey::generate().public();
        let member = SecretKey::generate().public();
        let mut value = config(member);
        value.query_timeout_secs = MAX_CAPACITY_QUERY_LIFETIME_SECS + 1;
        assert!(value.validate(own).is_err());

        let mut value = config(member);
        value.max_pending_queries = 0;
        assert!(value.validate(own).is_err());

        let mut value = config(member);
        value.max_agent_fanout = value.max_attached_agents + 1;
        assert!(value.validate(own).is_err());
    }
}
