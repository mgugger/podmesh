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

const SUPPORTED_POD_KINDS: [&str; 2] = ["Pod", "Deployment"];
const SUPPORTED_AUXILIARY_KINDS: [&str; 2] = ["Service", "Ingress"];

/// Default CPU limit to inject when manifest omits resources.limits.cpu
pub const DEFAULT_CPU_LIMIT: &str = "100m";
/// Default memory limit to inject when manifest omits resources.limits.memory
pub const DEFAULT_MEMORY_LIMIT: &str = "128Mi";
/// Default ephemeral storage limit to inject when omitted.
pub const DEFAULT_STORAGE_LIMIT: &str = "1Gi";
/// Default CPU request to inject when manifest omits resources.requests.cpu
pub const DEFAULT_CPU_REQUEST: &str = "50m";
/// Default memory request to inject when manifest omits resources.requests.memory
pub const DEFAULT_MEMORY_REQUEST: &str = "64Mi";
/// Default ephemeral storage request to inject when omitted.
pub const DEFAULT_STORAGE_REQUEST: &str = "512Mi";

/// Name of the sidecar container podmesh injects. It is the only container in
/// a pod allowed to hold `NET_ADMIN`, because it programs the egress redirect.
pub const INJECTED_SIDECAR_NAME: &str = "podmesh-sidecar";

/// Built-in Rego policy enforcing podmesh's pod security constraints.
///
/// The rule set is deny-by-default and deliberately narrow for the first
/// release: an agent runs tenant workloads against a Podman socket that is
/// equivalent to host control, so anything that could reach the host, another
/// tenant's containers, or the node's namespaces is refused outright rather
/// than filtered. Notably **no volumes of any kind are permitted**, because a
/// single `hostPath` entry is a complete container escape and the allow-listing
/// needed to admit the safe subset is not yet built.
///
/// Every rule inspects `all_containers`, which includes `initContainers` and
/// `ephemeralContainers`. Checking only `spec.containers` would leave the
/// obvious bypass of putting the privileged work in an init container.
const BUILTIN_POLICY: &str = r#"
package podmesh.policy

import rego.v1

default allow := false

allow if count(violations) == 0

pod_spec := input.spec if input.kind == "Pod"

pod_spec := input.spec.template.spec if {
    input.kind in ["Deployment", "ReplicaSet", "DaemonSet", "StatefulSet", "Job", "CronJob"]
}

input_containers := object.get(pod_spec, "containers", [])

init_containers := object.get(pod_spec, "initContainers", [])

ephemeral_containers := object.get(pod_spec, "ephemeralContainers", [])

all_containers := array.concat(
    array.concat(input_containers, init_containers),
    ephemeral_containers,
)

is_injected_sidecar(container) if container.name == "podmesh-sidecar"

# --- container security context -------------------------------------------

violations contains msg if {
    some container in all_containers
    container.securityContext.privileged == true
    msg := sprintf("container '%s' requests privileged: true", [container.name])
}

violations contains msg if {
    some container in all_containers
    container.securityContext.allowPrivilegeEscalation == true
    msg := sprintf("container '%s' requests allowPrivilegeEscalation: true", [container.name])
}

violations contains msg if {
    some container in all_containers
    not is_injected_sidecar(container)
    some capability in object.get(object.get(object.get(container, "securityContext", {}), "capabilities", {}), "add", [])
    msg := sprintf("container '%s' adds capability %s; only the injected sidecar may add capabilities", [container.name, capability])
}

violations contains msg if {
    some container in all_containers
    is_injected_sidecar(container)
    some capability in object.get(object.get(object.get(container, "securityContext", {}), "capabilities", {}), "add", [])
    capability != "NET_ADMIN"
    msg := sprintf("injected sidecar may only add NET_ADMIN, not %s", [capability])
}

violations contains msg if {
    some container in all_containers
    container.securityContext.runAsUser == 0
    msg := sprintf("container '%s' requests runAsUser: 0", [container.name])
}

violations contains msg if {
    some container in all_containers
    container.securityContext.runAsNonRoot == false
    msg := sprintf("container '%s' sets runAsNonRoot: false", [container.name])
}

violations contains msg if {
    some container in all_containers
    object.get(container, "securityContext", {}).procMount == "Unmasked"
    msg := sprintf("container '%s' requests an unmasked /proc", [container.name])
}

# --- pod-level namespace sharing ------------------------------------------

violations contains msg if {
    pod_spec.hostNetwork == true
    msg := "pod requests hostNetwork"
}

violations contains msg if {
    pod_spec.hostPID == true
    msg := "pod requests hostPID"
}

violations contains msg if {
    pod_spec.hostIPC == true
    msg := "pod requests hostIPC"
}

violations contains msg if {
    pod_spec.shareProcessNamespace == true
    msg := "pod requests shareProcessNamespace, which exposes the sidecar's environment to the application"
}

violations contains msg if {
    pod_spec.securityContext.runAsUser == 0
    msg := "pod securityContext requests runAsUser: 0"
}

violations contains msg if {
    pod_spec.securityContext.runAsNonRoot == false
    msg := "pod securityContext sets runAsNonRoot: false"
}

violations contains msg if {
    pod_spec.hostUsers == true
    msg := "pod requests hostUsers, disabling user-namespace isolation"
}

# --- host ports ------------------------------------------------------------

violations contains msg if {
    some container in all_containers
    some port in object.get(container, "ports", [])
    object.get(port, "hostPort", 0) != 0
    msg := sprintf("container '%s' requests hostPort %v", [container.name, port.hostPort])
}

# --- volumes ---------------------------------------------------------------
# No volume type is permitted yet. hostPath alone is a container escape, and
# the projected/CSI/PVC types need per-tenant naming rules that do not exist.

violations contains msg if {
    count(object.get(pod_spec, "volumes", [])) > 0
    msg := "pod declares volumes; volume mounts are not supported yet"
}

violations contains msg if {
    some container in all_containers
    count(object.get(container, "volumeMounts", [])) > 0
    not is_injected_sidecar(container)
    msg := sprintf("container '%s' declares volumeMounts; volume mounts are not supported yet", [container.name])
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
        validate_document_shape(&docs, &mut all_violations);
        if !all_violations.is_empty() {
            all_allowed = false;
        }

        // Validate each document
        for (idx, doc) in docs.iter().enumerate() {
            let json_value: JsonValue = serde_json::to_value(doc)
                .context(format!("failed to convert document {} to JSON", idx))?;

            // Set input for policy evaluation
            let input = regorus::Value::from_json_str(&json_value.to_string())
                .context("failed to create regorus input")?;
            self.engine.set_input(input);

            // Evaluate allow rule
            let allow_result = self
                .engine
                .eval_rule(String::from("data.podmesh.policy.allow"))
                .context("failed to evaluate allow rule")?;

            let allowed = matches!(allow_result, regorus::Value::Bool(true));
            if !allowed {
                all_allowed = false;
            }

            // Get violations
            let violations_result = self
                .engine
                .eval_rule(String::from("data.podmesh.policy.violations"))
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
                format!(
                    "manifest rejected by policy: {}",
                    result.violations.join("; ")
                )
            };
            return Err(anyhow!(msg));
        }

        result
            .mutated_manifest
            .ok_or_else(|| anyhow!("internal error: allowed manifest has no mutation result"))
    }
}

fn validate_document_shape(documents: &[serde_yaml::Value], violations: &mut Vec<String>) {
    let mut pod_documents = 0usize;
    for (index, document) in documents.iter().enumerate() {
        let Some(kind) = document.get("kind").and_then(serde_yaml::Value::as_str) else {
            violations.push(format!("document {index}: kind is required"));
            continue;
        };
        if SUPPORTED_POD_KINDS.contains(&kind) {
            pod_documents = pod_documents.saturating_add(1);
            let Some(spec) = pod_spec(document, kind) else {
                violations.push(format!(
                    "document {index}: {kind} does not contain a pod specification"
                ));
                continue;
            };
            if spec
                .get("ephemeralContainers")
                .and_then(serde_yaml::Value::as_sequence)
                .is_some_and(|containers| !containers.is_empty())
            {
                violations.push(format!(
                    "document {index}: ephemeralContainers are not supported"
                ));
            }
            if kind == "Deployment"
                && let Some(replicas) = document.get("spec").and_then(|value| value.get("replicas"))
                && replicas.as_u64() != Some(1)
            {
                violations.push(format!(
                    "document {index}: Deployment spec.replicas must be exactly 1"
                ));
            }
        } else if !SUPPORTED_AUXILIARY_KINDS.contains(&kind) {
            violations.push(format!(
                "document {index}: manifest kind {kind} is not supported"
            ));
        }
    }
    if pod_documents != 1 {
        violations.push(format!(
            "manifest must contain exactly one Pod or Deployment document, found {pod_documents}"
        ));
    }
}

fn pod_spec<'a>(document: &'a serde_yaml::Value, kind: &str) -> Option<&'a serde_yaml::Value> {
    match kind {
        "Pod" => document.get("spec"),
        "Deployment" => document
            .get("spec")
            .and_then(|spec| spec.get("template"))
            .and_then(|template| template.get("spec")),
        _ => None,
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
///
/// Init containers are mutated too. They consume the same CPU and memory as
/// regular containers, so leaving them unlimited would let a workload use far
/// more than the reservation the agent accounted for.
fn mutate_document_defaults(doc: &mut serde_yaml::Value) {
    for field in CONTAINER_FIELDS {
        if let Some(containers) = containers_mut(doc, field) {
            for container in containers {
                inject_resource_defaults(container);
            }
        }
    }
}

/// Container lists a pod spec may carry. Every one of them runs tenant code.
pub const CONTAINER_FIELDS: [&str; 2] = ["containers", "initContainers"];

/// Get a mutable reference to one container list based on manifest kind.
fn containers_mut<'a>(
    doc: &'a mut serde_yaml::Value,
    field: &str,
) -> Option<&'a mut Vec<serde_yaml::Value>> {
    let kind = doc
        .as_mapping()
        .and_then(|m| m.get(serde_yaml::Value::String("kind".to_string())))
        .and_then(|v| v.as_str())?;

    let spec = match kind {
        "Pod" => doc.get_mut("spec")?,
        "Deployment" => doc.get_mut("spec")?.get_mut("template")?.get_mut("spec")?,
        _ => return None,
    };

    spec.get_mut(field)?.as_sequence_mut()
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
        container_map.insert(
            resources_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }

    let resources = match container_map
        .get_mut(&resources_key)
        .and_then(|v| v.as_mapping_mut())
    {
        Some(r) => r,
        None => return,
    };

    // Inject limits
    let limits_key = serde_yaml::Value::String("limits".to_string());
    if !resources.contains_key(&limits_key) {
        resources.insert(
            limits_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    if let Some(limits) = resources
        .get_mut(&limits_key)
        .and_then(|v| v.as_mapping_mut())
    {
        let cpu_key = serde_yaml::Value::String("cpu".to_string());
        let memory_key = serde_yaml::Value::String("memory".to_string());
        let storage_key = serde_yaml::Value::String("ephemeral-storage".to_string());

        if !limits.contains_key(&cpu_key) {
            limits.insert(
                cpu_key,
                serde_yaml::Value::String(DEFAULT_CPU_LIMIT.to_string()),
            );
            debug!(
                "Injected default CPU limit for container '{}'",
                container_name
            );
        }
        if !limits.contains_key(&memory_key) {
            limits.insert(
                memory_key,
                serde_yaml::Value::String(DEFAULT_MEMORY_LIMIT.to_string()),
            );
            debug!(
                "Injected default memory limit for container '{}'",
                container_name
            );
        }
        if !limits.contains_key(&storage_key) {
            limits.insert(
                storage_key,
                serde_yaml::Value::String(DEFAULT_STORAGE_LIMIT.to_string()),
            );
        }
    }

    // Inject requests
    let requests_key = serde_yaml::Value::String("requests".to_string());
    if !resources.contains_key(&requests_key) {
        resources.insert(
            requests_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    if let Some(requests) = resources
        .get_mut(&requests_key)
        .and_then(|v| v.as_mapping_mut())
    {
        let cpu_key = serde_yaml::Value::String("cpu".to_string());
        let memory_key = serde_yaml::Value::String("memory".to_string());
        let storage_key = serde_yaml::Value::String("ephemeral-storage".to_string());

        if !requests.contains_key(&cpu_key) {
            requests.insert(
                cpu_key,
                serde_yaml::Value::String(DEFAULT_CPU_REQUEST.to_string()),
            );
        }
        if !requests.contains_key(&memory_key) {
            requests.insert(
                memory_key,
                serde_yaml::Value::String(DEFAULT_MEMORY_REQUEST.to_string()),
            );
        }
        if !requests.contains_key(&storage_key) {
            requests.insert(
                storage_key,
                serde_yaml::Value::String(DEFAULT_STORAGE_REQUEST.to_string()),
            );
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
  - name: podmesh-sidecar
    image: podmesh/sidecar:latest
    securityContext:
      capabilities:
        add:
        - NET_ADMIN
"#;

    const TENANT_CONTAINER_NAMED_SIDECAR: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: impostor
spec:
  containers:
  - name: sidecar
    image: nginx:latest
    securityContext:
      capabilities:
        add:
        - NET_ADMIN
"#;

    const PRIVILEGED_INIT_CONTAINER: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: sneaky
spec:
  initContainers:
  - name: setup
    image: busybox:latest
    securityContext:
      privileged: true
  containers:
  - name: app
    image: nginx:latest
"#;

    const HOST_NAMESPACE_POD: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: hostile
spec:
  hostNetwork: true
  hostPID: true
  shareProcessNamespace: true
  containers:
  - name: app
    image: nginx:latest
"#;

    const HOST_PATH_VOLUME_POD: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: escape
spec:
  volumes:
  - name: host-root
    hostPath:
      path: /
  containers:
  - name: app
    image: nginx:latest
    volumeMounts:
    - name: host-root
      mountPath: /host
"#;

    #[test]
    fn test_valid_pod_allowed() {
        let result = validate_manifest(VALID_POD).expect("validation should succeed");
        assert!(result.allowed, "valid pod should be allowed");
        assert!(result.violations.is_empty(), "should have no violations");
        assert!(
            result.mutated_manifest.is_some(),
            "should have mutated manifest"
        );
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
            result
                .violations
                .iter()
                .any(|v| v.contains("CAP_NET_ADMIN") || v.contains("NET_ADMIN")),
            "should mention NET_ADMIN in violations"
        );
    }

    #[test]
    fn test_authorized_sidecar_net_admin_allowed() {
        let result =
            validate_manifest(AUTHORIZED_SIDECAR_NET_ADMIN).expect("validation should succeed");
        assert!(
            result.allowed,
            "the injected sidecar may hold NET_ADMIN: {:?}",
            result.violations
        );
    }

    #[test]
    fn a_tenant_container_named_sidecar_cannot_take_capabilities() {
        // Only the container podmesh injects is exempt. Matching on a shorter
        // name would let a tenant name its own container `sidecar` and inherit
        // the exemption.
        let result =
            validate_manifest(TENANT_CONTAINER_NAMED_SIDECAR).expect("validation should succeed");
        assert!(!result.allowed, "violations: {:?}", result.violations);
        assert!(result.violations.iter().any(|v| v.contains("NET_ADMIN")));
    }

    #[test]
    fn a_privileged_init_container_is_rejected() {
        let result =
            validate_manifest(PRIVILEGED_INIT_CONTAINER).expect("validation should succeed");
        assert!(!result.allowed, "violations: {:?}", result.violations);
        assert!(result.violations.iter().any(|v| v.contains("privileged")));
    }

    #[test]
    fn host_namespace_sharing_is_rejected() {
        let result = validate_manifest(HOST_NAMESPACE_POD).expect("validation should succeed");
        assert!(!result.allowed);
        for expected in ["hostNetwork", "hostPID", "shareProcessNamespace"] {
            assert!(
                result.violations.iter().any(|v| v.contains(expected)),
                "expected a {expected} violation in {:?}",
                result.violations
            );
        }
    }

    #[test]
    fn volumes_are_rejected_outright() {
        let result = validate_manifest(HOST_PATH_VOLUME_POD).expect("validation should succeed");
        assert!(!result.allowed);
        assert!(
            result.violations.iter().any(|v| v.contains("volume")),
            "violations: {:?}",
            result.violations
        );
    }

    #[test]
    fn init_container_resources_are_measured() {
        let manifest = r#"
apiVersion: v1
kind: Pod
metadata:
  name: measured
spec:
  initContainers:
  - name: setup
    image: busybox:latest
  containers:
  - name: app
    image: nginx:latest
"#;
        let (_, resources) =
            crate::validate_and_measure_manifest(manifest.as_bytes()).expect("measure");
        // Two containers at the injected default of 100m each.
        assert_eq!(resources.cpu_milli, 200);
    }

    #[test]
    fn test_resource_defaults_injected() {
        let result = validate_manifest(VALID_POD).expect("validation should succeed");
        let mutated = result
            .mutated_manifest
            .expect("should have mutated manifest");

        // Check that defaults were injected
        assert!(mutated.contains("cpu:"), "should inject CPU");
        assert!(mutated.contains("memory:"), "should inject memory");
        assert!(
            mutated.contains("ephemeral-storage:"),
            "should inject ephemeral storage"
        );
        assert!(
            mutated.contains(DEFAULT_CPU_LIMIT) || mutated.contains("100m"),
            "should have default CPU limit"
        );
        assert!(
            mutated.contains(DEFAULT_MEMORY_LIMIT) || mutated.contains("128Mi"),
            "should have default memory limit"
        );
    }

    #[test]
    fn test_validate_and_mutate_success() {
        let mutated = validate_and_mutate_manifest(VALID_POD).expect("should succeed");
        assert!(
            mutated.contains("resources"),
            "should have resources section"
        );
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

    #[test]
    fn supported_auxiliary_documents_are_allowed() {
        let manifest = format!(
            r#"{VALID_POD}
---
apiVersion: v1
kind: Service
metadata:
  name: test
spec:
  selector:
    app: test
  ports:
  - port: 80
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: test
spec:
  rules: []
"#
        );
        let result = validate_manifest(&manifest).expect("validation should succeed");
        assert!(result.allowed, "violations: {:?}", result.violations);
    }

    #[test]
    fn unsupported_manifest_kind_is_rejected() {
        let manifest = r#"
apiVersion: batch/v1
kind: Job
metadata:
  name: unsupported
spec:
  template:
    spec:
      containers:
      - name: app
        image: busybox:latest
"#;
        let result = validate_manifest(manifest).expect("validation should succeed");
        assert!(!result.allowed);
        assert!(
            result
                .violations
                .iter()
                .any(|violation| violation.contains("Job is not supported")),
            "violations: {:?}",
            result.violations
        );
    }

    #[test]
    fn multiple_pod_bearing_documents_are_rejected() {
        let manifest = format!("{VALID_POD}\n---\n{VALID_POD}");
        let result = validate_manifest(&manifest).expect("validation should succeed");
        assert!(!result.allowed);
        assert!(
            result
                .violations
                .iter()
                .any(|violation| violation.contains("found 2")),
            "violations: {:?}",
            result.violations
        );
    }

    #[test]
    fn deployment_replica_count_must_be_one() {
        for replicas in ["0", "2", "\"1\""] {
            let manifest = format!(
                r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: invalid-replicas
spec:
  replicas: {replicas}
  template:
    spec:
      containers:
      - name: app
        image: nginx:latest
"#
            );
            let result = validate_manifest(&manifest).expect("validation should succeed");
            assert!(!result.allowed, "replicas={replicas}");
            assert!(
                result
                    .violations
                    .iter()
                    .any(|violation| violation.contains("replicas must be exactly 1")),
                "replicas={replicas}, violations: {:?}",
                result.violations
            );
        }
    }

    #[test]
    fn ephemeral_containers_are_rejected() {
        let manifest = r#"
apiVersion: v1
kind: Pod
metadata:
  name: ephemeral
spec:
  containers:
  - name: app
    image: nginx:latest
  ephemeralContainers:
  - name: debugger
    image: busybox:latest
"#;
        let result = validate_manifest(manifest).expect("validation should succeed");
        assert!(!result.allowed);
        assert!(
            result
                .violations
                .iter()
                .any(|violation| violation.contains("ephemeralContainers")),
            "violations: {:?}",
            result.violations
        );
    }
}
