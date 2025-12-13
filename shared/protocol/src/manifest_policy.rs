//! Manifest policy validation using OPA Rego policies via regorus.
//!
//! This module provides policy-based validation and mutation of Kubernetes manifests.
//! Built-in rules enforce security constraints:
//! - Reject privileged containers
//! - Only allow CAP_NET_ADMIN on containers named "sidecar"
//! - Inject default resource limits if missing
//!
//! Custom policies can be loaded from Rego files.

use anyhow::{Context, Result, anyhow};
use log::{debug, info};
use serde_json::Value as JsonValue;

/// Default CPU limit to inject when manifest omits resources.limits.cpu
pub const DEFAULT_CPU_LIMIT: &str = "100m";
/// Default memory limit to inject when manifest omits resources.limits.memory
pub const DEFAULT_MEMORY_LIMIT: &str = "128Mi";
/// Default CPU request to inject when manifest omits resources.requests.cpu
pub const DEFAULT_CPU_REQUEST: &str = "50m";
/// Default memory request to inject when manifest omits resources.requests.memory
pub const DEFAULT_MEMORY_REQUEST: &str = "64Mi";

/// Built-in Rego policy that enforces podmesh security constraints.
const BUILTIN_POLICY: &str = r#"
package podmesh.policy

import rego.v1

# Default deny
default allow := false

# Allow if all checks pass
allow if {
    not privileged_container_found
    not unauthorized_net_admin
}

# Check for privileged containers
privileged_container_found if {
    some container in input_containers
    container.securityContext.privileged == true
}

# Check for unauthorized CAP_NET_ADMIN
unauthorized_net_admin if {
    some container in input_containers
    has_net_admin(container)
    not is_sidecar_container(container)
}

# Sidecar containers are allowed to have NET_ADMIN
is_sidecar_container(container) if {
    container.name == "sidecar"
}

is_sidecar_container(container) if {
    container.name == "podmesh-sidecar"
}

# Helper to check if container has CAP_NET_ADMIN
has_net_admin(container) if {
    some cap in container.securityContext.capabilities.add
    cap == "NET_ADMIN"
}

# Get all containers from various manifest types
input_containers := containers if {
    input.kind == "Pod"
    containers := input.spec.containers
}

input_containers := containers if {
    input.kind in ["Deployment", "ReplicaSet", "DaemonSet", "StatefulSet"]
    containers := input.spec.template.spec.containers
}

# Collect all violations for detailed error messages
violations contains msg if {
    some container in input_containers
    container.securityContext.privileged == true
    msg := sprintf("container '%s' has privileged: true", [container.name])
}

violations contains msg if {
    some container in input_containers
    has_net_admin(container)
    not is_sidecar_container(container)
    msg := sprintf("container '%s' has CAP_NET_ADMIN but only sidecar is allowed", [container.name])
}
"#;

/// Result of policy validation with potential mutations.
#[derive(Debug, Clone)]
pub struct PolicyResult {
    /// Whether the manifest passed all policy checks
    pub allowed: bool,
    /// List of policy violations (if any)
    pub violations: Vec<String>,
    /// The potentially mutated manifest (with injected defaults)
    pub mutated_manifest: Option<String>,
}

/// Policy engine for validating and mutating Kubernetes manifests.
pub struct PolicyEngine {
    engine: regorus::Engine,
}

impl PolicyEngine {
    /// Create a new policy engine with built-in rules.
    pub fn new() -> Result<Self> {
        let mut engine = regorus::Engine::new();
        
        engine
            .add_policy(String::from("builtin.rego"), String::from(BUILTIN_POLICY))
            .context("failed to add built-in policy")?;
        
        debug!("PolicyEngine initialized with built-in rules");
        Ok(Self { engine })
    }

    /// Add a custom Rego policy from a string.
    pub fn add_policy(&mut self, name: &str, policy: &str) -> Result<()> {
        self.engine
            .add_policy(name.to_string(), policy.to_string())
            .context(format!("failed to add policy '{}'", name))?;
        info!("Added custom policy: {}", name);
        Ok(())
    }

    /// Validate a manifest against policies.
    /// Returns the policy result with violations and mutated manifest.
    pub fn validate(&mut self, manifest_yaml: &str) -> Result<PolicyResult> {
        // Parse YAML to JSON for policy evaluation
        let docs = crate::manifest_yaml::parse_yaml_documents_from_str(manifest_yaml)
            .context("failed to parse manifest YAML")?;

        if docs.is_empty() {
            return Err(anyhow!("manifest contains no documents"));
        }

        let mut all_violations = Vec::new();
        let mut all_allowed = true;

        // Validate each document
        for (idx, doc) in docs.iter().enumerate() {
            let json_value: JsonValue = serde_json::to_value(doc)
                .context(format!("failed to convert document {} to JSON", idx))?;

            // Set input for policy evaluation
            let input = regorus::Value::from_json_str(&json_value.to_string())
                .context("failed to create regorus input")?;
            self.engine.set_input(input);

            // Evaluate allow rule
            let allow_result = self.engine.eval_rule(String::from("data.podmesh.policy.allow"))
                .context("failed to evaluate allow rule")?;
            
            let allowed = matches!(allow_result, regorus::Value::Bool(true));
            if !allowed {
                all_allowed = false;
            }

            // Get violations
            let violations_result = self.engine.eval_rule(String::from("data.podmesh.policy.violations"))
                .context("failed to evaluate violations rule")?;
            
            if let regorus::Value::Set(violations) = violations_result {
                for v in violations.iter() {
                    if let regorus::Value::String(msg) = v {
                        let violation = if docs.len() > 1 {
                            format!("document {}: {}", idx, msg.as_ref())
                        } else {
                            msg.to_string()
                        };
                        all_violations.push(violation);
                    }
                }
            }
        }

        // Mutate manifest to inject defaults if allowed
        let mutated_manifest = if all_allowed {
            Some(mutate_manifest_defaults(manifest_yaml)?)
        } else {
            None
        };

        Ok(PolicyResult {
            allowed: all_allowed,
            violations: all_violations,
            mutated_manifest,
        })
    }

    /// Validate and return the mutated manifest, or error with violations.
    pub fn validate_and_mutate(&mut self, manifest_yaml: &str) -> Result<String> {
        let result = self.validate(manifest_yaml)?;
        
        if !result.allowed {
            let msg = if result.violations.is_empty() {
                "manifest rejected by policy".to_string()
            } else {
                format!("manifest rejected by policy: {}", result.violations.join("; "))
            };
            return Err(anyhow!(msg));
        }

        result.mutated_manifest
            .ok_or_else(|| anyhow!("internal error: allowed manifest has no mutation result"))
    }
}

impl Default for PolicyEngine {
    fn default() -> Self {
        Self::new().expect("failed to create default PolicyEngine")
    }
}

/// Mutate a manifest to inject default resource limits where missing.
fn mutate_manifest_defaults(manifest_yaml: &str) -> Result<String> {
    let mut docs = crate::manifest_yaml::parse_yaml_documents_from_str(manifest_yaml)
        .context("failed to parse manifest for mutation")?;

    for doc in docs.iter_mut() {
        mutate_document_defaults(doc);
    }

    crate::manifest_yaml::serialize_yaml_documents(&docs)
        .context("failed to serialize mutated manifest")
}

/// Mutate a single document to inject defaults.
fn mutate_document_defaults(doc: &mut serde_yaml::Value) {
    let containers = get_containers_mut(doc);
    if let Some(containers) = containers {
        for container in containers {
            inject_resource_defaults(container);
        }
    }
}

/// Get mutable reference to containers array based on manifest kind.
fn get_containers_mut(doc: &mut serde_yaml::Value) -> Option<&mut Vec<serde_yaml::Value>> {
    let kind = doc
        .as_mapping()
        .and_then(|m| m.get(&serde_yaml::Value::String("kind".to_string())))
        .and_then(|v| v.as_str())?;

    let spec = match kind {
        "Pod" => doc.get_mut("spec")?,
        "Deployment" | "ReplicaSet" | "DaemonSet" | "StatefulSet" => {
            doc.get_mut("spec")?
                .get_mut("template")?
                .get_mut("spec")?
        }
        _ => return None,
    };

    spec.get_mut("containers")?.as_sequence_mut()
}

/// Inject default resource requests and limits into a container if missing.
fn inject_resource_defaults(container: &mut serde_yaml::Value) {
    // Extract container name first to avoid borrow conflicts
    let container_name = container
        .get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let container_map = match container.as_mapping_mut() {
        Some(m) => m,
        None => return,
    };

    // Ensure resources key exists
    let resources_key = serde_yaml::Value::String("resources".to_string());
    if !container_map.contains_key(&resources_key) {
        container_map.insert(resources_key.clone(), serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    }

    let resources = match container_map.get_mut(&resources_key).and_then(|v| v.as_mapping_mut()) {
        Some(r) => r,
        None => return,
    };

    // Inject limits
    let limits_key = serde_yaml::Value::String("limits".to_string());
    if !resources.contains_key(&limits_key) {
        resources.insert(limits_key.clone(), serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    }
    if let Some(limits) = resources.get_mut(&limits_key).and_then(|v| v.as_mapping_mut()) {
        let cpu_key = serde_yaml::Value::String("cpu".to_string());
        let memory_key = serde_yaml::Value::String("memory".to_string());
        
        if !limits.contains_key(&cpu_key) {
            limits.insert(cpu_key, serde_yaml::Value::String(DEFAULT_CPU_LIMIT.to_string()));
            debug!("Injected default CPU limit for container '{}'", container_name);
        }
        if !limits.contains_key(&memory_key) {
            limits.insert(memory_key, serde_yaml::Value::String(DEFAULT_MEMORY_LIMIT.to_string()));
            debug!("Injected default memory limit for container '{}'", container_name);
        }
    }

    // Inject requests
    let requests_key = serde_yaml::Value::String("requests".to_string());
    if !resources.contains_key(&requests_key) {
        resources.insert(requests_key.clone(), serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    }
    if let Some(requests) = resources.get_mut(&requests_key).and_then(|v| v.as_mapping_mut()) {
        let cpu_key = serde_yaml::Value::String("cpu".to_string());
        let memory_key = serde_yaml::Value::String("memory".to_string());
        
        if !requests.contains_key(&cpu_key) {
            requests.insert(cpu_key, serde_yaml::Value::String(DEFAULT_CPU_REQUEST.to_string()));
        }
        if !requests.contains_key(&memory_key) {
            requests.insert(memory_key, serde_yaml::Value::String(DEFAULT_MEMORY_REQUEST.to_string()));
        }
    }
}

/// Validate a manifest using the default policy engine.
/// Convenience function for one-off validation.
pub fn validate_manifest(manifest_yaml: &str) -> Result<PolicyResult> {
    let mut engine = PolicyEngine::new()?;
    engine.validate(manifest_yaml)
}

/// Validate and mutate a manifest using the default policy engine.
/// Returns the mutated manifest on success, or error with violations.
pub fn validate_and_mutate_manifest(manifest_yaml: &str) -> Result<String> {
    let mut engine = PolicyEngine::new()?;
    engine.validate_and_mutate(manifest_yaml)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_POD: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: test-pod
spec:
  containers:
  - name: nginx
    image: nginx:latest
"#;

    const PRIVILEGED_POD: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: privileged-pod
spec:
  containers:
  - name: nginx
    image: nginx:latest
    securityContext:
      privileged: true
"#;

    const UNAUTHORIZED_NET_ADMIN: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: net-admin-pod
spec:
  containers:
  - name: nginx
    image: nginx:latest
    securityContext:
      capabilities:
        add:
        - NET_ADMIN
"#;

    const AUTHORIZED_SIDECAR_NET_ADMIN: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: sidecar-pod
spec:
  containers:
  - name: app
    image: nginx:latest
  - name: sidecar
    image: podmesh/sidecar:latest
    securityContext:
      capabilities:
        add:
        - NET_ADMIN
"#;

    #[test]
    fn test_valid_pod_allowed() {
        let result = validate_manifest(VALID_POD).expect("validation should succeed");
        assert!(result.allowed, "valid pod should be allowed");
        assert!(result.violations.is_empty(), "should have no violations");
        assert!(result.mutated_manifest.is_some(), "should have mutated manifest");
    }

    #[test]
    fn test_privileged_container_rejected() {
        let result = validate_manifest(PRIVILEGED_POD).expect("validation should succeed");
        assert!(!result.allowed, "privileged pod should be rejected");
        assert!(!result.violations.is_empty(), "should have violations");
        assert!(
            result.violations.iter().any(|v| v.contains("privileged")),
            "should mention privileged in violations"
        );
    }

    #[test]
    fn test_unauthorized_net_admin_rejected() {
        let result = validate_manifest(UNAUTHORIZED_NET_ADMIN).expect("validation should succeed");
        assert!(!result.allowed, "unauthorized NET_ADMIN should be rejected");
        assert!(
            result.violations.iter().any(|v| v.contains("CAP_NET_ADMIN") || v.contains("NET_ADMIN")),
            "should mention NET_ADMIN in violations"
        );
    }

    #[test]
    fn test_authorized_sidecar_net_admin_allowed() {
        let result = validate_manifest(AUTHORIZED_SIDECAR_NET_ADMIN).expect("validation should succeed");
        assert!(result.allowed, "sidecar with NET_ADMIN should be allowed");
        assert!(result.violations.is_empty(), "should have no violations");
    }

    #[test]
    fn test_resource_defaults_injected() {
        let result = validate_manifest(VALID_POD).expect("validation should succeed");
        let mutated = result.mutated_manifest.expect("should have mutated manifest");
        
        // Check that defaults were injected
        assert!(mutated.contains("cpu:"), "should inject CPU");
        assert!(mutated.contains("memory:"), "should inject memory");
        assert!(mutated.contains(DEFAULT_CPU_LIMIT) || mutated.contains("100m"), "should have default CPU limit");
        assert!(mutated.contains(DEFAULT_MEMORY_LIMIT) || mutated.contains("128Mi"), "should have default memory limit");
    }

    #[test]
    fn test_validate_and_mutate_success() {
        let mutated = validate_and_mutate_manifest(VALID_POD).expect("should succeed");
        assert!(mutated.contains("resources"), "should have resources section");
    }

    #[test]
    fn test_validate_and_mutate_failure() {
        let result = validate_and_mutate_manifest(PRIVILEGED_POD);
        assert!(result.is_err(), "should fail for privileged pod");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("rejected"), "error should mention rejection");
    }

    #[test]
    fn test_deployment_validation() {
        let deployment = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-deployment
spec:
  replicas: 1
  selector:
    matchLabels:
      app: test
  template:
    metadata:
      labels:
        app: test
    spec:
      containers:
      - name: nginx
        image: nginx:latest
"#;
        let result = validate_manifest(deployment).expect("validation should succeed");
        assert!(result.allowed, "valid deployment should be allowed");
    }
}
