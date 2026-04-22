//! k8s → podmesh manifest conversion.

use serde::Deserialize as _;
use protocol::podmesh_annotations::PodmeshAnnotations;

const UNSUPPORTED_KINDS: &[(&str, &str)] = &[
    ("PersistentVolumeClaim", "PVC not supported — podmesh has no distributed storage layer"),
    ("HorizontalPodAutoscaler", "HPA not supported — use podmesh.io/replicas annotation"),
    ("DaemonSet", "DaemonSet not supported — podmesh uses rendezvous placement"),
];

const UNSUPPORTED_SERVICE_TYPES: &[&str] = &["ClusterIP", "NodePort", "LoadBalancer"];

/// Convert a Kubernetes manifest YAML to podmesh-annotated YAML.
/// Returns the converted YAML string and a list of warnings.
pub fn convert_manifest(yaml_str: &str) -> anyhow::Result<(String, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut output_docs = Vec::new();

    for doc in serde_yaml::Deserializer::from_str(yaml_str) {
        let mut value: serde_yaml::Value = serde_yaml::Value::deserialize(doc)?;
        let kind = value.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // Check for unsupported kinds
        for (unsupported_kind, message) in UNSUPPORTED_KINDS {
            if kind == *unsupported_kind {
                warnings.push(format!("WARNING: {} — {}", kind, message));
            }
        }

        // Check for unsupported Service types
        if kind == "Service" {
            if let Some(svc_type) = value.get("spec").and_then(|s| s.get("type")).and_then(|t| t.as_str()) {
                if UNSUPPORTED_SERVICE_TYPES.contains(&svc_type) {
                    warnings.push(format!(
                        "WARNING: Service type '{}' not supported — sidecar auto-registers routes with proxy",
                        svc_type
                    ));
                }
            }
        }

        // Check for ConfigMap/Secret
        if kind == "ConfigMap" {
            warnings.push("WARNING: ConfigMap — seal secrets inside the workload spec using podmesh encryption".to_string());
        }
        if kind == "Secret" {
            warnings.push("WARNING: Secret — seal secrets inside the workload spec using podmesh encryption".to_string());
        }

        // Inject default podmesh annotations for Pod/Deployment kinds
        if matches!(kind.as_str(), "Pod" | "Deployment" | "ReplicaSet" | "StatefulSet") {
            let metadata = value.get_mut("metadata")
                .and_then(|m| m.as_mapping_mut());
            if let Some(meta) = metadata {
                let annotations = meta
                    .entry(serde_yaml::Value::String("annotations".to_string()))
                    .or_insert(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
                if let Some(ann_map) = annotations.as_mapping_mut() {
                    // Only inject if not already present
                    let defaults = [
                        (PodmeshAnnotations::REPLICAS, "1"),
                        (PodmeshAnnotations::LEASE_TTL, "60"),
                        (PodmeshAnnotations::CUSTODIAN_COUNT, "3"),
                        (PodmeshAnnotations::CUSTODIAN_THRESHOLD, "2"),
                        (PodmeshAnnotations::SUBMISSION_VERSION, "1"),
                    ];
                    for (key, default_val) in defaults {
                        let k = serde_yaml::Value::String(key.to_string());
                        if !ann_map.contains_key(&k) {
                            ann_map.insert(k, serde_yaml::Value::String(default_val.to_string()));
                        }
                    }
                }
            }
        }

        output_docs.push(serde_yaml::to_string(&value)?);
    }

    Ok((output_docs.join("---\n"), warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN_POD: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: my-app
spec:
  containers:
  - name: app
    image: nginx:latest
"#;

    const PVC_MANIFEST: &str = r#"
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: my-pvc
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
"#;

    const HPA_MANIFEST: &str = r#"
apiVersion: autoscaling/v1
kind: HorizontalPodAutoscaler
metadata:
  name: my-hpa
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: my-app
  minReplicas: 1
  maxReplicas: 5
"#;

    #[test]
    fn test_convert_adds_default_podmesh_annotations() {
        let (output, warnings) = convert_manifest(PLAIN_POD).unwrap();
        assert!(output.contains("podmesh.io/replicas"));
        assert!(output.contains("podmesh.io/custodian-count"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_convert_warns_on_pvc() {
        let (_, warnings) = convert_manifest(PVC_MANIFEST).unwrap();
        assert!(warnings.iter().any(|w| w.contains("PersistentVolumeClaim")));
    }

    #[test]
    fn test_convert_warns_on_hpa() {
        let (_, warnings) = convert_manifest(HPA_MANIFEST).unwrap();
        assert!(warnings.iter().any(|w| w.contains("HorizontalPodAutoscaler")));
    }

    #[test]
    fn test_convert_passthrough_for_plain_pod() {
        let (output, _) = convert_manifest(PLAIN_POD).unwrap();
        // Original pod content preserved
        assert!(output.contains("my-app"));
        assert!(output.contains("nginx:latest"));
    }
}
