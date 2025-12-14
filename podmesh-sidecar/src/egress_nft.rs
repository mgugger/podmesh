//! nftables configuration for transparent egress proxy using rustables
//!
//! Sets up NAT rules to redirect outbound TCP/UDP traffic to the local
//! transparent proxy listener. Excludes pod-local networks and loopback.
//!
//! Uses rustables crate which communicates directly via netlink,
//! eliminating the need for the `nft` CLI binary.

use anyhow::{Context, Result};
use ipnetwork::IpNetwork;
use rustables::{
    expr::{Cmp, CmpOp, Immediate, Meta, MetaType, Nat, NatType, Register, VerdictKind},
    Batch, Chain, ChainPolicy, ChainType, Hook, HookClass, MsgType, Protocol, ProtocolFamily, Rule,
    Table,
};

/// Table name for podmesh egress rules
const TABLE_NAME: &str = "podmesh_egress";

/// Chain name for output NAT rules
const OUTPUT_CHAIN: &str = "output";

/// Transparent proxy port where sidecar listens
const PROXY_PORT: u16 = 15001;

/// Sidecar UID used to exclude our own traffic from redirection
/// This must match the UID the sidecar runs as inside the container
const SIDECAR_UID: u32 = 1337;

/// Networks to exclude from egress interception
const EXCLUDED_NETWORKS: &[&str] = &[
    "127.0.0.0/8",    // Loopback
    "10.0.2.0/24",    // Pasta/slirp4netns default
    "169.254.0.0/16", // Link-local
];

/// Configuration for egress nftables setup
#[derive(Debug, Clone)]
pub struct EgressNftConfig {
    /// Port to redirect traffic to
    pub proxy_port: u16,
    /// UID of the sidecar process (to exclude from redirection)
    pub sidecar_uid: u32,
    /// Additional networks to exclude from redirection
    pub excluded_networks: Vec<IpNetwork>,
}

impl Default for EgressNftConfig {
    fn default() -> Self {
        let mut excluded = Vec::new();
        for net in EXCLUDED_NETWORKS {
            if let Ok(n) = net.parse() {
                excluded.push(n);
            }
        }
        Self {
            proxy_port: PROXY_PORT,
            sidecar_uid: SIDECAR_UID,
            excluded_networks: excluded,
        }
    }
}

/// Sets up nftables rules for transparent egress proxy using rustables
///
/// Creates a nat table with an OUTPUT chain that redirects all outbound
/// TCP/UDP traffic to the local proxy port, excluding:
/// - Traffic from the sidecar's own UID
/// - Traffic to excluded networks (loopback, pod-local)
///
/// Note: rustables doesn't have a direct "redirect" statement, so we use
/// DNAT to 127.0.0.1:port which achieves the same effect.
pub fn setup_egress_rules(config: &EgressNftConfig) -> Result<()> {
    log::info!(
        "Setting up egress nftables rules via netlink: proxy_port={}, sidecar_uid={}",
        config.proxy_port,
        config.sidecar_uid
    );

    // Try to delete existing table first (ignore errors if it doesn't exist)
    let _ = try_delete_existing_table();

    let mut batch = Batch::new();

    // Create the table
    let table = Table::new(ProtocolFamily::Ipv4).with_name(TABLE_NAME);
    batch.add(&table, MsgType::Add);

    // Create the OUTPUT chain (type nat, hook output, priority -100)
    // Chain::new returns Chain directly (not Result)
    let chain = Chain::new(&table)
        .with_name(OUTPUT_CHAIN)
        .with_type(ChainType::Nat)
        .with_hook(Hook::new(HookClass::Out, -100))
        .with_policy(ChainPolicy::Accept);
    batch.add(&chain, MsgType::Add);

    // Rule 2: Skip traffic from sidecar's own UID
    // meta skuid <sidecar_uid> accept
    let skip_uid_rule = Rule::new(&chain)?
        .with_expr(Meta::new(MetaType::SkUid))
        .with_expr(Cmp::new(CmpOp::Eq, config.sidecar_uid.to_ne_bytes()))
        .with_expr(Immediate::new_verdict(VerdictKind::Accept));
    batch.add(&skip_uid_rule, MsgType::Add);

    // Rule 3: Skip traffic to excluded networks
    for network in &config.excluded_networks {
        let skip_net_rule = build_skip_network_rule(&chain, network)?;
        batch.add(&skip_net_rule, MsgType::Add);
    }

    // Rule 4: Redirect TCP to proxy port using DNAT
    // Note: We only redirect TCP traffic because the egress proxy is TCP-only.
    // UDP traffic (including DNS) is allowed to pass through normally.
    let tcp_redirect_rule = build_redirect_rule(&chain, Protocol::TCP, config.proxy_port)?;
    batch.add(&tcp_redirect_rule, MsgType::Add);

    // Apply the ruleset via netlink
    batch
        .send()
        .context("Failed to apply nftables rules via netlink")?;

    log::info!("Egress nftables rules applied successfully via netlink");
    Ok(())
}

/// Builds a rule to skip traffic destined for a specific network
fn build_skip_network_rule(chain: &Chain, network: &IpNetwork) -> Result<Rule> {
    // Using the high-level dnetwork helper from rustables
    let rule = Rule::new(chain)?
        .dnetwork(*network)?
        .with_expr(Immediate::new_verdict(VerdictKind::Accept));

    Ok(rule)
}

/// Builds a redirect rule for a specific protocol
///
/// Since rustables doesn't have a direct redirect statement, we use DNAT
/// to redirect to 127.0.0.1:port. For OUTPUT chain redirect, this is
/// equivalent to the nft "redirect to :port" statement.
fn build_redirect_rule(chain: &Chain, protocol: Protocol, port: u16) -> Result<Rule> {
    // Load loopback IP (127.0.0.1) into register for NAT destination
    let loopback_ip: std::net::Ipv4Addr = "127.0.0.1".parse().unwrap();
    let ip_bytes = loopback_ip.octets().to_vec();

    // Load port value into register for NAT
    let port_bytes = port.to_be_bytes().to_vec();

    let rule = Rule::new(chain)?
        // Match protocol (TCP or UDP)
        .protocol(protocol)
        // Load destination IP into Reg1
        .with_expr(Immediate::new_data(ip_bytes, Register::Reg1))
        // Load destination port into Reg2
        .with_expr(Immediate::new_data(port_bytes, Register::Reg2))
        // Apply DNAT with IP from Reg1 and port from Reg2
        .with_expr(
            Nat::default()
                .with_nat_type(NatType::DNat)
                .with_family(ProtocolFamily::Ipv4)
                .with_ip_register(Register::Reg1)
                .with_port_register(Register::Reg2),
        );

    Ok(rule)
}

/// Removes the podmesh egress nftables rules
pub fn cleanup_egress_rules() -> Result<()> {
    log::info!("Cleaning up egress nftables rules");

    let mut batch = Batch::new();

    // Delete the entire table
    let table = Table::new(ProtocolFamily::Ipv4).with_name(TABLE_NAME);
    batch.add(&table, MsgType::Del);

    batch
        .send()
        .context("Failed to cleanup egress nftables rules")?;

    log::info!("Egress nftables rules cleaned up");
    Ok(())
}

/// Attempts to delete existing table, ignoring errors if it doesn't exist
fn try_delete_existing_table() -> Result<()> {
    let mut batch = Batch::new();

    let table = Table::new(ProtocolFamily::Ipv4).with_name(TABLE_NAME);
    batch.add(&table, MsgType::Del);

    batch.send().context("Failed to delete existing table")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = EgressNftConfig::default();
        assert_eq!(config.proxy_port, PROXY_PORT);
        assert_eq!(config.sidecar_uid, SIDECAR_UID);
        assert!(!config.excluded_networks.is_empty());
    }

    #[test]
    fn test_excluded_networks_parsed() {
        let config = EgressNftConfig::default();
        // Check that loopback is in excluded networks
        let has_loopback = config
            .excluded_networks
            .iter()
            .any(|n| n.to_string() == "127.0.0.0/8");
        assert!(has_loopback);
    }
}
