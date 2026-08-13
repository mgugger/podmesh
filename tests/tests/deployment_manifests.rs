//! The shipped deploy manifests must actually describe the topology the
//! local-deployment spec calls for, and must never carry a pre-created secret.
//!
//! These assertions run in CI on every change, which is the point: the manifests
//! are the thing an operator copies, and a silent regression in them is not
//! visible from any Rust test that only exercises the code.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

const ROOTFUL: &str = include_str!("../../deploy/podmesh_rootful.yml");
const ROOTLESS: &str = include_str!("../../deploy/podmesh_rootless.yml");

fn documents(name: &str, manifest: &str) -> Result<Vec<serde_yaml::Value>> {
    serde_yaml::Deserializer::from_str(manifest)
        .map(|document| {
            serde_yaml::Value::deserialize(document)
                .with_context(|| format!("parse {name} deployment document"))
        })
        .collect()
}

/// Every container across every document, as `(name, container)`.
fn containers(documents: &[serde_yaml::Value]) -> Vec<(String, &serde_yaml::Value)> {
    documents
        .iter()
        .filter_map(|document| document.get("spec")?.get("containers")?.as_sequence())
        .flatten()
        .filter_map(|container| {
            let name = container.get("name")?.as_str()?.to_string();
            Some((name, container))
        })
        .collect()
}

fn env_of(container: &serde_yaml::Value) -> BTreeMap<String, String> {
    container
        .get("env")
        .and_then(serde_yaml::Value::as_sequence)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    Some((
                        entry.get("name")?.as_str()?.to_string(),
                        entry
                            .get("value")
                            .and_then(serde_yaml::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn deployment_manifests_are_valid_multi_document_yaml() -> Result<()> {
    for (name, manifest) in [("rootful", ROOTFUL), ("rootless", ROOTLESS)] {
        let documents = documents(name, manifest)?;
        ensure!(
            !documents.is_empty(),
            "{name} deployment must contain at least one document"
        );
        ensure!(
            documents.iter().all(|document| document
                .get("kind")
                .and_then(serde_yaml::Value::as_str)
                .is_some()),
            "{name} deployment contains a document without kind"
        );
    }
    Ok(())
}

#[test]
fn each_manifest_runs_three_schedulers_three_agents_and_three_proxies() -> Result<()> {
    for (name, manifest) in [("rootful", ROOTFUL), ("rootless", ROOTLESS)] {
        let documents = documents(name, manifest)?;
        let names: Vec<String> = containers(&documents)
            .into_iter()
            .map(|(container, _)| container)
            .collect();
        for (role, expected) in [("scheduler", 3), ("agent", 3), ("proxy", 3)] {
            let count = names
                .iter()
                .filter(|container| container.contains(role))
                .count();
            ensure!(
                count == expected,
                "{name} deployment has {count} {role} containers, expected {expected}"
            );
        }
    }
    Ok(())
}

/// The sample topology deliberately attaches no agent to the first scheduler,
/// two to the second and one to the third, so a client always exercises the
/// case where the scheduler it reaches holds none of the agents it needs.
#[test]
fn agents_are_spread_unevenly_across_schedulers() -> Result<()> {
    for (name, manifest) in [("rootful", ROOTFUL), ("rootless", ROOTLESS)] {
        let documents = documents(name, manifest)?;
        let mut attachments: BTreeMap<String, usize> = BTreeMap::new();
        for (container, value) in containers(&documents) {
            if !container.contains("agent") {
                continue;
            }
            let url = env_of(value)
                .get("PODMESH_AGENT_SCHEDULER_URLS")
                .cloned()
                .with_context(|| format!("{name}: {container} has no scheduler URL"))?;
            *attachments.entry(url).or_default() += 1;
        }
        let mut counts: Vec<usize> = attachments.values().copied().collect();
        counts.sort_unstable();
        ensure!(
            counts == vec![1, 2],
            "{name} deployment must attach agents to exactly two schedulers as 1 and 2, got {counts:?}"
        );
    }
    Ok(())
}

#[test]
fn the_manifests_reference_no_pre_created_secret() -> Result<()> {
    for (name, manifest) in [("rootful", ROOTFUL), ("rootless", ROOTLESS)] {
        let documents = documents(name, manifest)?;
        ensure!(
            documents.iter().all(|document| document.get("kind")
                != Some(&serde_yaml::Value::String("Secret".into()))),
            "{name} deployment declares a Secret; credentials must be self-provisioned"
        );
        ensure!(
            !manifest.contains("secretKeyRef"),
            "{name} deployment reads a Secret; credentials must be self-provisioned"
        );
    }
    Ok(())
}

#[test]
fn published_host_ports_do_not_collide() -> Result<()> {
    for (name, manifest) in [("rootful", ROOTFUL), ("rootless", ROOTLESS)] {
        let documents = documents(name, manifest)?;
        let mut seen: BTreeMap<u64, String> = BTreeMap::new();
        for (container, value) in containers(&documents) {
            let Some(ports) = value.get("ports").and_then(serde_yaml::Value::as_sequence) else {
                continue;
            };
            for port in ports {
                let Some(host_port) = port.get("hostPort").and_then(serde_yaml::Value::as_u64)
                else {
                    continue;
                };
                if let Some(previous) = seen.insert(host_port, container.clone()) {
                    anyhow::bail!(
                        "{name} deployment publishes host port {host_port} from both \
                         {previous} and {container}"
                    );
                }
            }
        }
    }
    Ok(())
}

/// The relay bootstrap endpoint hands a live relay credential in cleartext to
/// anyone who can reach it. Peers adopt that token over the pod network, so
/// exactly one proxy needs to serve it and the others must not re-publish it.
#[test]
fn exactly_one_proxy_serves_the_relay_bootstrap_token() -> Result<()> {
    for (name, manifest) in [("rootful", ROOTFUL), ("rootless", ROOTLESS)] {
        let documents = documents(name, manifest)?;
        let publishers: Vec<String> = containers(&documents)
            .into_iter()
            .filter(|(_, value)| {
                env_of(value)
                    .get("PODMESH_PROXY_PUBLISH_RELAY_BOOTSTRAP")
                    .map(String::as_str)
                    == Some("true")
            })
            .map(|(container, _)| container)
            .collect();
        ensure!(
            publishers.len() == 1,
            "{name} deployment has {} proxies serving the relay bootstrap token ({publishers:?}), \
             expected exactly one",
            publishers.len()
        );
    }
    Ok(())
}
