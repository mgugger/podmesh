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
    Batch, Chain, ChainPolicy, ChainType, Hook, HookClass, MsgType, Protocol, ProtocolFamily, Rule,
    Table,
    expr::{Cmp, CmpOp, Immediate, Meta, MetaType, Nat, NatType, Register, VerdictKind},
};
use std::fs;

/// Table name for podmesh egress rules
const TABLE_NAME: &str = "podmesh_egress";
const DNS_V6_TABLE: &str = "podmesh_dns_v6";
pub const DNS_BYPASS_MARK: u32 = 0x504d;
pub const DNS_PORT: u16 = 15053;

/// Chain name for output NAT rules
const OUTPUT_CHAIN: &str = "output";

/// Transparent proxy port where sidecar listens
const PROXY_PORT: u16 = 15001;

/// Networks to exclude from egress interception
const EXCLUDED_NETWORKS: &[&str] = &[
    "127.0.0.0/8",    // Loopback
    "10.0.2.0/24",    // Pasta/slirp4netns default
    "169.254.0.0/16", // Link-local
];

/// Capability bit for CAP_NET_ADMIN in the /proc status mask
const CAP_NET_ADMIN_BIT: u32 = 12;

/// Configuration for egress nftables setup
#[derive(Debug, Clone)]
pub struct EgressNftConfig {
    /// Port to redirect traffic to
    pub proxy_port: u16,
    /// UID of the sidecar process (to exclude from redirection)
    pub sidecar_uid: u32,
    /// Additional networks to exclude from redirection
    pub excluded_networks: Vec<IpNetwork>,
    pub dns_port: Option<u16>,
    pub ipv6_dns: bool,
    pub redirect_external: bool,
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
            sidecar_uid: unsafe { libc::geteuid() },
            excluded_networks: excluded,
            dns_port: None,
            ipv6_dns: false,
            redirect_external: true,
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
    if config.dns_port.is_some() {
        validate_vip_routes(
            &fs::read_to_string("/proc/net/route").context("read workload IPv4 routes")?,
        )?;
    }
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

    // DNS and service VIP interception precede UID/network exemptions: an app
    // sharing the sidecar UID must still use local split DNS and peer routing.
    let bypass = Rule::new(&chain)?
        .with_expr(Meta::new(MetaType::Mark))
        .with_expr(Cmp::new(CmpOp::Eq, DNS_BYPASS_MARK.to_ne_bytes()))
        .with_expr(Immediate::new_verdict(VerdictKind::Accept));
    batch.add(&bypass, MsgType::Add);
    if let Some(port) = config.dns_port {
        for protocol in [Protocol::UDP, Protocol::TCP] {
            let rule = redirect_rule(Rule::new(&chain)?.dport(53, protocol), port);
            batch.add(&rule, MsgType::Add);
        }
        let vip = redirect_rule(
            Rule::new(&chain)?
                .protocol(Protocol::TCP)
                .dnetwork("198.18.0.0/24".parse::<IpNetwork>()?)?,
            config.proxy_port,
        );
        batch.add(&vip, MsgType::Add);
    }

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
    if config.redirect_external {
        let tcp_redirect_rule = build_redirect_rule(&chain, Protocol::TCP, config.proxy_port)?;
        batch.add(&tcp_redirect_rule, MsgType::Add);
    }
    if config.ipv6_dns
        && let Some(port) = config.dns_port
    {
        let table = Table::new(ProtocolFamily::Ipv6).with_name(DNS_V6_TABLE);
        batch.add(&table, MsgType::Add);
        let chain = Chain::new(&table)
            .with_name(OUTPUT_CHAIN)
            .with_type(ChainType::Nat)
            .with_hook(Hook::new(HookClass::Out, -100))
            .with_policy(ChainPolicy::Accept);
        batch.add(&chain, MsgType::Add);
        let bypass = Rule::new(&chain)?
            .with_expr(Meta::new(MetaType::Mark))
            .with_expr(Cmp::new(CmpOp::Eq, DNS_BYPASS_MARK.to_ne_bytes()))
            .with_expr(Immediate::new_verdict(VerdictKind::Accept));
        batch.add(&bypass, MsgType::Add);
        for protocol in [Protocol::UDP, Protocol::TCP] {
            let rule = Rule::new(&chain)?
                .dport(53, protocol)
                .with_expr(Immediate::new_data(
                    std::net::Ipv6Addr::LOCALHOST.octets().to_vec(),
                    Register::Reg1,
                ))
                .with_expr(Immediate::new_data(
                    port.to_be_bytes().to_vec(),
                    Register::Reg2,
                ))
                .with_expr(
                    Nat::default()
                        .with_nat_type(NatType::DNat)
                        .with_family(ProtocolFamily::Ipv6)
                        .with_ip_register(Register::Reg1)
                        .with_port_register(Register::Reg2),
                );
            batch.add(&rule, MsgType::Add);
        }
    }

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
    Ok(redirect_rule(Rule::new(chain)?.protocol(protocol), port))
}

// All matching expressions must precede register loads and the terminal NAT
// action. Appending dport/dnetwork after DNAT redirects unrelated traffic.
fn redirect_rule(rule: Rule, port: u16) -> Rule {
    // Load loopback IP (127.0.0.1) into register for NAT destination
    let loopback_ip: std::net::Ipv4Addr = "127.0.0.1".parse().unwrap();
    let ip_bytes = loopback_ip.octets().to_vec();

    // Load port value into register for NAT
    let port_bytes = port.to_be_bytes().to_vec();

    rule
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
        )
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
    let _ = delete_v6_table();

    log::info!("Egress nftables rules cleaned up");
    Ok(())
}

/// Attempts to delete existing table, ignoring errors if it doesn't exist
fn try_delete_existing_table() -> Result<()> {
    let mut batch = Batch::new();

    let table = Table::new(ProtocolFamily::Ipv4).with_name(TABLE_NAME);
    batch.add(&table, MsgType::Del);

    let _ = delete_v6_table();
    batch.send().context("Failed to delete existing table")
}

pub(crate) fn validate_vip_routes(routes: &str) -> Result<()> {
    let vip = u32::from(std::net::Ipv4Addr::new(198, 18, 0, 0));
    let vip_mask = 0xffffff00;
    for line in routes.lines().skip(1) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 8 {
            continue;
        }
        let network = u32::from_str_radix(fields[1], 16).context("invalid route destination")?;
        let mask = u32::from_str_radix(fields[7], 16).context("invalid route mask")?;
        let network = u32::from_be_bytes(network.to_le_bytes());
        let mask = u32::from_be_bytes(mask.to_le_bytes());
        if mask == 0 {
            continue;
        }
        anyhow::ensure!(
            (network & mask != vip & mask) && (network & vip_mask != vip),
            "service VIP range conflicts with a workload route"
        );
    }
    Ok(())
}
fn delete_v6_table() -> Result<()> {
    let mut batch = Batch::new();
    batch.add(
        &Table::new(ProtocolFamily::Ipv6).with_name(DNS_V6_TABLE),
        MsgType::Del,
    );
    batch.send().context("delete IPv6 DNS table")
}

/// Returns true when the current process has CAP_NET_ADMIN in the effective set
pub fn has_net_admin_capability() -> bool {
    match fs::read_to_string("/proc/self/status") {
        Ok(contents) => {
            if let Some(mask) = parse_cap_eff_mask(&contents) {
                return mask_has_net_admin(mask);
            }
            false
        }
        Err(err) => {
            log::debug!(
                "failed to read /proc/self/status to check capabilities: {}",
                err
            );
            false
        }
    }
}

fn parse_cap_eff_mask(contents: &str) -> Option<u64> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|rest| {
            let value = rest.trim();
            u64::from_str_radix(value, 16).ok()
        })
}

fn mask_has_net_admin(mask: u64) -> bool {
    mask & (1u64 << CAP_NET_ADMIN_BIT) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_vip_route_conflicts_but_not_default_routes() {
        let header = "Iface Destination Gateway Flags RefCnt Use Metric Mask\n";
        assert!(validate_vip_routes(&format!("{header}eth0 00000000 0100000A 0003 0 0 0 00000000\neth0 0000020A 00000000 0001 0 0 0 00FFFFFF\n")).is_ok());
        assert!(
            validate_vip_routes(&format!(
                "{header}eth0 000012C6 00000000 0001 0 0 0 00FFFFFF\n"
            ))
            .is_err()
        );
    }
    #[test]
    fn dns_and_vip_filters_precede_nat_and_register_loads() -> Result<()> {
        let table = Table::new(ProtocolFamily::Ipv4).with_name(TABLE_NAME);
        let chain = Chain::new(&table).with_name(OUTPUT_CHAIN);
        for protocol in [Protocol::UDP, Protocol::TCP] {
            let rule = redirect_rule(Rule::new(&chain)?.dport(53, protocol), DNS_PORT);
            let names = rule
                .get_expressions()
                .unwrap()
                .iter()
                .map(|expression| expression.get_name().unwrap().as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                names,
                [
                    "meta",
                    "cmp",
                    "payload",
                    "cmp",
                    "immediate",
                    "immediate",
                    "nat"
                ]
            );
        }
        let vip = redirect_rule(
            Rule::new(&chain)?
                .protocol(Protocol::TCP)
                .dnetwork("198.18.0.0/24".parse::<IpNetwork>()?)?,
            PROXY_PORT,
        );
        let names = vip
            .get_expressions()
            .unwrap()
            .iter()
            .map(|expression| expression.get_name().unwrap().as_str())
            .collect::<Vec<_>>();
        assert_eq!(&names[names.len() - 3..], ["immediate", "immediate", "nat"]);
        assert!(names[..names.len() - 3].contains(&"cmp"));
        Ok(())
    }

    #[test]
    fn test_default_config() {
        let config = EgressNftConfig::default();
        assert_eq!(config.proxy_port, PROXY_PORT);
        assert_eq!(config.sidecar_uid, unsafe { libc::geteuid() });
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

    #[test]
    fn parses_cap_eff_mask_values() {
        let sample = "Name:\ttest\nCapEff:\t0000000000001000\n";
        let mask = super::parse_cap_eff_mask(sample).expect("mask parse");
        assert!(super::mask_has_net_admin(mask));

        let sample_no_cap = "CapEff:\t0000000000000000\n";
        let mask = super::parse_cap_eff_mask(sample_no_cap).expect("mask parse");
        assert!(!super::mask_has_net_admin(mask));
    }
}
