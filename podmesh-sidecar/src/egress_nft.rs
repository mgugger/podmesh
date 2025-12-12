//! nftables configuration for transparent egress proxy
//!
//! Sets up NAT rules to redirect outbound TCP/UDP traffic to the local
//! transparent proxy listener. Excludes pod-local networks and loopback.

use anyhow::{Context, Result};
use ipnetwork::IpNetwork;
use nftables::{
    batch::Batch,
    expr::{Expression, Meta, MetaKey, NamedExpression, Payload, PayloadField},
    helper,
    schema::{Chain, NfCmd, NfListObject, Rule, Table},
    stmt::{Match, Operator, Statement, NAT},
    types::{NfChainPolicy, NfChainType, NfFamily, NfHook},
};
use std::borrow::Cow;

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

/// Sets up nftables rules for transparent egress proxy
///
/// Creates a nat table with an OUTPUT chain that redirects all outbound
/// TCP/UDP traffic to the local proxy port, excluding:
/// - Traffic from the sidecar's own UID
/// - Traffic to excluded networks (loopback, pod-local)
pub fn setup_egress_rules(config: &EgressNftConfig) -> Result<()> {
    log::info!(
        "Setting up egress nftables rules: proxy_port={}, sidecar_uid={}",
        config.proxy_port,
        config.sidecar_uid
    );

    // Try to delete existing table first (ignore errors if it doesn't exist)
    let _ = try_delete_existing_table();

    let mut batch = Batch::new();

    // Create the table
    batch.add(NfListObject::Table(Table {
        family: NfFamily::IP,
        name: Cow::Borrowed(TABLE_NAME),
        handle: None,
    }));

    // Create the OUTPUT chain (type nat, hook output, priority -100)
    batch.add(NfListObject::Chain(Chain {
        family: NfFamily::IP,
        table: Cow::Borrowed(TABLE_NAME),
        name: Cow::Borrowed(OUTPUT_CHAIN),
        _type: Some(NfChainType::NAT),
        hook: Some(NfHook::Output),
        prio: Some(-100),
        policy: Some(NfChainPolicy::Accept),
        ..Default::default()
    }));

    // Rule 1: Skip traffic from sidecar's own UID
    // meta skuid <sidecar_uid> accept
    batch.add(NfListObject::Rule(Rule {
        family: NfFamily::IP,
        table: Cow::Borrowed(TABLE_NAME),
        chain: Cow::Borrowed(OUTPUT_CHAIN),
        expr: vec![
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Meta(Meta {
                    key: MetaKey::Skuid,
                })),
                right: Expression::Number(config.sidecar_uid),
                op: Operator::EQ,
            }),
            Statement::Accept(None),
        ]
        .into(),
        handle: None,
        index: None,
        comment: Some(Cow::Borrowed("Skip sidecar's own traffic")),
    }));

    // Rule 2: Skip traffic to excluded networks
    for network in &config.excluded_networks {
        let prefix_str = format!("{}/{}", network.ip(), network.prefix());
        batch.add(NfListObject::Rule(Rule {
            family: NfFamily::IP,
            table: Cow::Borrowed(TABLE_NAME),
            chain: Cow::Borrowed(OUTPUT_CHAIN),
            expr: vec![
                Statement::Match(Match {
                    left: Expression::Named(NamedExpression::Payload(Payload::PayloadField(
                        PayloadField {
                            protocol: Cow::Borrowed("ip"),
                            field: Cow::Borrowed("daddr"),
                        },
                    ))),
                    right: Expression::String(Cow::Owned(prefix_str.clone())),
                    op: Operator::EQ,
                }),
                Statement::Accept(None),
            ]
            .into(),
            handle: None,
            index: None,
            comment: Some(Cow::Owned(format!("Skip traffic to {}", prefix_str))),
        }));
    }

    // Rule 3: Redirect TCP to proxy port
    batch.add(NfListObject::Rule(Rule {
        family: NfFamily::IP,
        table: Cow::Borrowed(TABLE_NAME),
        chain: Cow::Borrowed(OUTPUT_CHAIN),
        expr: vec![
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Meta(Meta {
                    key: MetaKey::L4proto,
                })),
                right: Expression::String(Cow::Borrowed("tcp")),
                op: Operator::EQ,
            }),
            Statement::Redirect(Some(NAT {
                addr: None,
                family: None,
                port: Some(Expression::Number(config.proxy_port as u32)),
                flags: None,
            })),
        ]
        .into(),
        handle: None,
        index: None,
        comment: Some(Cow::Borrowed("Redirect TCP to egress proxy")),
    }));

    // Rule 4: Redirect UDP to proxy port
    batch.add(NfListObject::Rule(Rule {
        family: NfFamily::IP,
        table: Cow::Borrowed(TABLE_NAME),
        chain: Cow::Borrowed(OUTPUT_CHAIN),
        expr: vec![
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Meta(Meta {
                    key: MetaKey::L4proto,
                })),
                right: Expression::String(Cow::Borrowed("udp")),
                op: Operator::EQ,
            }),
            Statement::Redirect(Some(NAT {
                addr: None,
                family: None,
                port: Some(Expression::Number(config.proxy_port as u32)),
                flags: None,
            })),
        ]
        .into(),
        handle: None,
        index: None,
        comment: Some(Cow::Borrowed("Redirect UDP to egress proxy")),
    }));

    // Apply the ruleset
    helper::apply_ruleset(&batch.to_nftables())
        .map_err(|e| anyhow::anyhow!("Failed to apply nftables rules: {}", e))?;

    log::info!("Egress nftables rules applied successfully");
    Ok(())
}

/// Removes the podmesh egress nftables rules
pub fn cleanup_egress_rules() -> Result<()> {
    log::info!("Cleaning up egress nftables rules");

    let mut batch = Batch::new();

    // Delete the entire table using NfCmd::Delete
    batch.add_cmd(NfCmd::Delete(NfListObject::Table(Table {
        family: NfFamily::IP,
        name: Cow::Borrowed(TABLE_NAME),
        handle: None,
    })));

    helper::apply_ruleset(&batch.to_nftables())
        .context("Failed to cleanup egress nftables rules")?;

    log::info!("Egress nftables rules cleaned up");
    Ok(())
}

/// Attempts to delete existing table, ignoring errors if it doesn't exist
fn try_delete_existing_table() -> Result<()> {
    let mut batch = Batch::new();
    batch.add_cmd(NfCmd::Delete(NfListObject::Table(Table {
        family: NfFamily::IP,
        name: Cow::Borrowed(TABLE_NAME),
        handle: None,
    })));
    helper::apply_ruleset(&batch.to_nftables())
        .context("Failed to delete existing table")
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
