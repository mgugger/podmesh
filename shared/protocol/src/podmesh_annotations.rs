//! Parser for podmesh.io/* Kubernetes annotations.

use serde::{Deserialize, Serialize};

/// Parsed podmesh annotations from a Kubernetes manifest.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PodmeshAnnotations {
    /// Rego trust policy source
    pub trust_policy: Option<String>,
    /// Desired replica count
    pub replicas: Option<u32>,
    /// Lease TTL in seconds
    pub lease_ttl_secs: Option<u32>,
}

impl PodmeshAnnotations {
    pub const TRUST_POLICY: &'static str = "podmesh.io/trust-policy";
    pub const REPLICAS: &'static str = "podmesh.io/replicas";
    pub const LEASE_TTL: &'static str = "podmesh.io/lease-ttl";

    /// Parse annotations from a serde_yaml Value (the annotations map from a k8s manifest).
    pub fn from_yaml_annotations(annotations: &serde_yaml::Value) -> Self {
        let mut result = Self::default();

        let get = |key: &str| -> Option<String> {
            annotations
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };

        result.trust_policy = get(Self::TRUST_POLICY);
        result.replicas = get(Self::REPLICAS).and_then(|s| s.parse().ok());
        result.lease_ttl_secs = get(Self::LEASE_TTL).and_then(|s| s.parse().ok());

        result
    }

    /// Parse annotations from a Kubernetes manifest YAML string.
    /// Tries each document and returns the first non-empty annotation set found.
    pub fn from_manifest_yaml(yaml_str: &str) -> anyhow::Result<Self> {
        for doc in serde_yaml::Deserializer::from_str(yaml_str) {
            let value: serde_yaml::Value = serde_yaml::Value::deserialize(doc)?;
            if let Some(annotations) = value.get("metadata").and_then(|m| m.get("annotations")) {
                let parsed = Self::from_yaml_annotations(annotations);
                if parsed.trust_policy.is_some()
                    || parsed.replicas.is_some()
                    || parsed.lease_ttl_secs.is_some()
                {
                    return Ok(parsed);
                }
            }
        }
        Ok(Self::default())
    }

    /// Apply defaults for any missing values.
    pub fn with_defaults(mut self) -> Self {
        self.replicas.get_or_insert(1);
        self.lease_ttl_secs.get_or_insert(60);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANNOTATED_MANIFEST: &str = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
  annotations:
    podmesh.io/trust-policy: |
      allow { input.capabilities[_] == "gpu" }
    podmesh.io/replicas: "2"
    podmesh.io/lease-ttl: "120"
spec:
  template:
    spec:
      containers:
      - name: app
        image: nginx:latest
"#;

    const PLAIN_MANIFEST: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: plain-pod
spec:
  containers:
  - name: app
    image: nginx:latest
"#;

    #[test]
    fn test_annotations_parsed_correctly() {
        let ann = PodmeshAnnotations::from_manifest_yaml(ANNOTATED_MANIFEST).unwrap();
        assert!(ann.trust_policy.is_some());
        assert_eq!(ann.replicas, Some(2));
        assert_eq!(ann.lease_ttl_secs, Some(120));
    }

    #[test]
    fn test_annotations_override_cli_defaults() {
        let ann = PodmeshAnnotations::from_manifest_yaml(ANNOTATED_MANIFEST).unwrap();
        // When annotations are present, they should override defaults
        assert_eq!(ann.replicas, Some(2)); // not the default of 1
    }

    #[test]
    fn test_missing_annotations_use_defaults() {
        let ann = PodmeshAnnotations::from_manifest_yaml(PLAIN_MANIFEST)
            .unwrap()
            .with_defaults();
        assert_eq!(ann.replicas, Some(1));
    }

    #[test]
    fn test_trust_policy_missing_returns_none() {
        let ann = PodmeshAnnotations::from_manifest_yaml(PLAIN_MANIFEST).unwrap();
        assert!(ann.trust_policy.is_none());
    }
}
