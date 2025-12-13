//! Integration tests for manifest policy validation.
//!
//! These tests verify that the OPA-based policy validation correctly:
//! - Rejects privileged containers
//! - Rejects unauthorized NET_ADMIN capability
//! - Allows NET_ADMIN only for sidecar containers
//! - Injects default resource limits when missing

use anyhow::{Context, Result};
use protocol::manifest_policy::{validate_manifest, validate_and_mutate_manifest};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests crate must be inside workspace")
        .to_path_buf()
}

fn load_manifest(name: &str) -> Result<String> {
    let path = workspace_root().join("tests/sample_manifests").join(name);
    fs::read_to_string(&path)
        .with_context(|| format!("failed to read manifest: {}", path.display()))
}

#[test]
fn test_privileged_container_rejected() {
    let manifest = load_manifest("privileged_container.yml").unwrap();
    let result = validate_manifest(&manifest).expect("validation should succeed");
    
    assert!(!result.allowed, "privileged container should be rejected");
    assert!(!result.violations.is_empty(), "should have violations");
    assert!(
        result.violations.iter().any(|v| v.to_lowercase().contains("privileged")),
        "should mention 'privileged' in violations: {:?}",
        result.violations
    );
}

#[test]
fn test_unauthorized_net_admin_rejected() {
    let manifest = load_manifest("unauthorized_net_admin.yml").unwrap();
    let result = validate_manifest(&manifest).expect("validation should succeed");
    
    assert!(!result.allowed, "unauthorized NET_ADMIN should be rejected");
    assert!(
        result.violations.iter().any(|v| v.contains("NET_ADMIN") || v.contains("net_admin")),
        "should mention NET_ADMIN in violations: {:?}",
        result.violations
    );
}

#[test]
fn test_authorized_sidecar_net_admin_allowed() {
    let manifest = load_manifest("authorized_sidecar_net_admin.yml").unwrap();
    let result = validate_manifest(&manifest).expect("validation should succeed");
    
    assert!(
        result.allowed,
        "sidecar container with NET_ADMIN should be allowed: {:?}",
        result.violations
    );
}

#[test]
fn test_valid_deployment_allowed() {
    let manifest = load_manifest("demo_deployment_without_sidecar.yml").unwrap();
    let result = validate_manifest(&manifest).expect("validation should succeed");
    
    assert!(
        result.allowed,
        "valid deployment should be allowed: {:?}",
        result.violations
    );
}

#[test]
fn test_resource_defaults_injected() {
    let manifest = load_manifest("deployment_no_resources.yml").unwrap();
    let result = validate_and_mutate_manifest(&manifest);
    
    assert!(result.is_ok(), "should succeed: {:?}", result.err());
    let mutated = result.unwrap();
    
    // Parse the mutated manifest to verify resources were injected
    let mut docs: Vec<serde_yaml::Value> = Vec::new();
    for document in serde_yaml::Deserializer::from_str(&mutated) {
        let value: serde_yaml::Value = Deserialize::deserialize(document).unwrap();
        docs.push(value);
    }
    
    // Find the Deployment document
    let deployment = docs.iter()
        .find(|doc| {
            doc.get("kind")
                .and_then(|k| k.as_str())
                .map(|k| k == "Deployment")
                .unwrap_or(false)
        })
        .expect("should have a Deployment");
    
    // Navigate to container resources
    let containers = deployment
        .get("spec")
        .and_then(|s| s.get("template"))
        .and_then(|t| t.get("spec"))
        .and_then(|s| s.get("containers"))
        .and_then(|c| c.as_sequence())
        .expect("should have containers");
    
    assert!(!containers.is_empty(), "should have at least one container");
    
    let container = &containers[0];
    let resources = container.get("resources")
        .expect("resources should be injected");
    
    let limits = resources.get("limits").expect("limits should exist");
    let requests = resources.get("requests").expect("requests should exist");
    
    // Verify default values
    assert_eq!(
        limits.get("cpu").and_then(|v| v.as_str()),
        Some("100m"),
        "CPU limit should be 100m"
    );
    assert_eq!(
        limits.get("memory").and_then(|v| v.as_str()),
        Some("128Mi"),
        "memory limit should be 128Mi"
    );
    assert_eq!(
        requests.get("cpu").and_then(|v| v.as_str()),
        Some("50m"),
        "CPU request should be 50m"
    );
    assert_eq!(
        requests.get("memory").and_then(|v| v.as_str()),
        Some("64Mi"),
        "memory request should be 64Mi"
    );
}

#[test]
fn test_policy_preserves_existing_resources() {
    // A manifest with existing resource limits should not be modified
    let manifest = r#"
apiVersion: v1
kind: Pod
metadata:
  name: existing-resources
spec:
  containers:
    - name: app
      image: nginx
      resources:
        limits:
          cpu: "500m"
          memory: "512Mi"
        requests:
          cpu: "250m"
          memory: "256Mi"
"#;
    
    let result = validate_and_mutate_manifest(manifest);
    assert!(result.is_ok(), "should succeed: {:?}", result.err());
    
    let mutated = result.unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&mutated).unwrap();
    
    let container = doc
        .get("spec")
        .and_then(|s| s.get("containers"))
        .and_then(|c| c.as_sequence())
        .and_then(|c| c.first())
        .expect("should have container");
    
    let limits = container
        .get("resources")
        .and_then(|r| r.get("limits"))
        .expect("should have limits");
    
    // Verify existing values are preserved
    assert_eq!(
        limits.get("cpu").and_then(|v| v.as_str()),
        Some("500m"),
        "existing CPU limit should be preserved"
    );
    assert_eq!(
        limits.get("memory").and_then(|v| v.as_str()),
        Some("512Mi"),
        "existing memory limit should be preserved"
    );
}
