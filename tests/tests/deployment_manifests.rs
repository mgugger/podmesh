//! The shipped deploy manifests must actually describe the topology the
//! local-deployment spec calls for, and must never carry a pre-created secret.
//!
//! These assertions run in CI on every change, which is the point: the manifests
//! are the thing an operator copies, and a silent regression in them is not
//! visible from any Rust test that only exercises the code.

use std::{collections::BTreeMap, process::Command};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

const ROOTFUL: &str = include_str!("../../deploy/podmesh_rootful.yml");
const ROOTLESS: &str = include_str!("../../deploy/podmesh_rootless.yml");
const BUILD_CONTAINERS: &str = include_str!("../../deploy/build_containers.sh");
const CONTAINERFILE: &str = include_str!("../../deploy/Containerfile");

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

#[test]
fn application_metrics_ports_are_explicit_private_and_consistent() -> Result<()> {
    let expected = BTreeMap::from([
        ("scheduler-1", 9200_u64),
        ("scheduler-2", 9201),
        ("scheduler-3", 9202),
        ("agent-1", 9210),
        ("agent-2", 9211),
        ("agent-3", 9212),
        ("proxy-1", 9220),
        ("proxy-2", 9221),
        ("proxy-3", 9222),
    ]);

    for (manifest_name, manifest) in [("rootful", ROOTFUL), ("rootless", ROOTLESS)] {
        let documents = documents(manifest_name, manifest)?;
        for (container_name, expected_port) in &expected {
            let (_, container) = containers(&documents)
                .into_iter()
                .find(|(name, _)| name == container_name)
                .with_context(|| format!("{manifest_name}: missing {container_name}"))?;
            let environment = env_of(container);
            ensure!(
                environment.get("PODMESH_METRICS_LISTEN")
                    == Some(&format!("0.0.0.0:{expected_port}")),
                "{manifest_name}: {container_name} has the wrong application metrics address"
            );
            if container_name.starts_with("agent-") {
                ensure!(
                    environment.get("PODMESH_AGENT_SIDECAR_METRICS_LISTEN")
                        == Some(&"0.0.0.0:9230".to_string()),
                    "{manifest_name}: {container_name} does not propagate sidecar metrics"
                );
            }
            let metrics_port = container
                .get("ports")
                .and_then(serde_yaml::Value::as_sequence)
                .and_then(|ports| {
                    ports.iter().find(|port| {
                        port.get("containerPort")
                            .and_then(serde_yaml::Value::as_u64)
                            == Some(*expected_port)
                    })
                })
                .with_context(|| {
                    format!("{manifest_name}: {container_name} has no metrics container port")
                })?;
            ensure!(
                metrics_port.get("hostPort").is_none(),
                "{manifest_name}: {container_name} publishes unauthenticated metrics to the host"
            );
        }
        ensure!(
            containers(&documents).into_iter().all(|(name, _)| {
                !name.contains("prometheus") && !name.contains("metrics-helper")
            }),
            "{manifest_name}: metrics must not add a collector or helper container"
        );
    }
    Ok(())
}

#[test]
fn container_build_tags_are_scoped_validated_and_native_only() -> Result<()> {
    ensure!(
        BUILD_CONTAINERS.contains("IMAGE_TAG=${PODMESH_IMAGE_TAG:-latest}"),
        "container builds must preserve latest as the operator default"
    );
    ensure!(
        BUILD_CONTAINERS.contains("podmesh/scheduler:$IMAGE_TAG")
            && BUILD_CONTAINERS.contains("podmesh/agent:$IMAGE_TAG")
            && BUILD_CONTAINERS.contains("podmesh/proxy:$IMAGE_TAG")
            && BUILD_CONTAINERS.contains("podmesh/sidecar:$IMAGE_TAG"),
        "all four runtime images must use the validated image tag"
    );
    ensure!(
        BUILD_CONTAINERS.contains("cross-architecture builds are unsupported"),
        "image build must retain native architecture refusal"
    );
    ensure!(
        BUILD_CONTAINERS.contains("podman image inspect --format '{{.Id}}'")
            && BUILD_CONTAINERS.contains("built $image=$image_id"),
        "container builds must report the built image identities"
    );

    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("integration package has no workspace parent")?;
    let result = Command::new("sh")
        .arg("deploy/build_containers.sh")
        .current_dir(workspace)
        .env("PODMESH_IMAGE_TAG", "../unsafe")
        .output()
        .context("run container tag validation")?;
    ensure!(
        !result.status.success()
            && String::from_utf8_lossy(&result.stderr).contains("invalid PODMESH_IMAGE_TAG"),
        "unsafe image tag was not refused before Podman build"
    );
    Ok(())
}

#[test]
fn final_images_copy_only_runtime_assets() -> Result<()> {
    let final_stages = CONTAINERFILE
        .split("FROM scratch AS ")
        .skip(1)
        .collect::<Vec<_>>();
    ensure!(
        final_stages.len() == 4,
        "Containerfile must have exactly four final scratch stages"
    );
    for stage in final_stages {
        let name = stage.lines().next().unwrap_or_default();
        ensure!(
            stage
                .lines()
                .filter(|line| line.starts_with("COPY "))
                .all(|line| {
                    line.contains("/etc/ssl/certs/ca-certificates.crt")
                        || line.contains("/out/podmesh-")
                        || line.contains("/out/podman")
                }),
            "final image {name} copies a non-runtime asset"
        );
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
