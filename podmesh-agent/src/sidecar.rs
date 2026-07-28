use anyhow::{Context, Result, anyhow};
use protocol::EndpointRecord;
use protocol::sidecar_metadata::{METADATA_BLOB_ENV_VAR, SidecarMetadata};
use serde_json::{Value, json};

const SIDECAR_NAME: &str = "podmesh-sidecar";
const RUNTIME_NAME_PREFIX: &str = "podmesh-";
const MAX_DNS_LABEL_LEN: usize = 63;
const PODMAN_WORKLOAD_SUFFIX_LEN: usize = "-pod".len();

pub fn workload_runtime_name(workload_id: &str) -> String {
    let digest = blake3::hash(workload_id.as_bytes()).to_hex();
    let digest_len = MAX_DNS_LABEL_LEN - PODMAN_WORKLOAD_SUFFIX_LEN - RUNTIME_NAME_PREFIX.len();
    format!("{RUNTIME_NAME_PREFIX}{}", &digest.as_str()[..digest_len])
}

pub fn inject(
    manifest: &[u8],
    workload_id: &str,
    namespace_id: &str,
    sidecar_image: &str,
    proxy_endpoints: &[EndpointRecord],
    workload_relay_auth_token: &str,
    workload_relay_ca_certificates: &[Vec<u8>],
) -> Result<Vec<u8>> {
    let original = manifest.to_vec();
    let metadata = SidecarMetadata {
        manifest_id: workload_id.to_string(),
        manifest_b64: crypto::b64_encode(&original),
        owner_public_key_b64: Some(namespace_id.to_string()),
        proxy_endpoints: proxy_endpoints.to_vec(),
        workload_relay_auth_token: workload_relay_auth_token.to_string(),
        workload_relay_ca_certificates: workload_relay_ca_certificates.to_vec(),
    };
    metadata.validate()?;
    let metadata_blob = crypto::b64_encode(&serde_json::to_vec(&metadata)?);
    let sidecar = json!({
        "name": SIDECAR_NAME,
        "image": sidecar_image,
        "imagePullPolicy": "IfNotPresent",
        "env": [
            { "name": METADATA_BLOB_ENV_VAR, "value": metadata_blob },
            { "name": "PODMESH_ENABLE_EGRESS", "value": "true" },
            { "name": "RUST_LOG", "value": "info" }
        ],
        "securityContext": {
            "capabilities": { "add": ["NET_ADMIN"] }
        }
    });
    let documents = protocol::manifest_yaml::parse_yaml_documents_from_slice(manifest)
        .context("decode canonical workload manifest")?;
    let mut injected = 0usize;
    let runtime_name = workload_runtime_name(workload_id);
    let mut output = Vec::with_capacity(documents.len());
    for document in documents {
        let mut value = serde_json::to_value(document)?;
        let is_pod = value.get("kind").and_then(Value::as_str) == Some("Pod");
        let has_pod_spec = if is_pod {
            value.get("spec").is_some()
        } else {
            value.pointer("/spec/template/spec").is_some()
        };
        if has_pod_spec {
            let metadata = value
                .get_mut("metadata")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| anyhow!("workload metadata must be an object"))?;
            metadata.insert("name".to_string(), Value::String(runtime_name.clone()));
            let pod_spec = if is_pod {
                value.get_mut("spec")
            } else {
                value.pointer_mut("/spec/template/spec")
            }
            .ok_or_else(|| anyhow!("workload pod spec disappeared during transformation"))?;
            let containers = pod_spec
                .as_object_mut()
                .and_then(|spec| spec.get_mut("containers"))
                .and_then(Value::as_array_mut)
                .ok_or_else(|| anyhow!("pod spec containers must be an array"))?;
            anyhow::ensure!(
                !containers
                    .iter()
                    .any(|container| container.get("name").and_then(Value::as_str)
                        == Some(SIDECAR_NAME)),
                "manifest already contains a podmesh sidecar"
            );
            containers.push(sidecar.clone());
            injected += 1;
        }
        output.push(serde_yaml::to_value(value)?);
    }
    anyhow::ensure!(
        injected > 0,
        "manifest does not contain a supported pod spec"
    );
    anyhow::ensure!(
        injected == 1,
        "a workload may contain only one pod-bearing document"
    );
    Ok(protocol::manifest_yaml::serialize_yaml_documents(&output)?.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_proxy_endpoints() -> Vec<EndpointRecord> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
        vec![
            EndpointRecord {
                version: protocol::ENDPOINT_RECORD_VERSION,
                endpoint_id: iroh::SecretKey::generate().public().as_bytes().to_vec(),
                relay_url: Some("https://relay.example.test".into()),
                direct_addresses: vec!["127.0.0.1:4002".into()],
                signing_pubkey: String::new(),
                issued_at_secs: now,
                expires_at_secs: now + 60,
                signature: String::new(),
            }
            .sign(&public, &private, now)
            .unwrap(),
        ]
    }

    #[test]
    fn injects_sidecar_into_deployment() {
        let manifest = br#"apiVersion: apps/v1
kind: Deployment
metadata: { name: demo }
spec:
    template:
        spec:
            containers:
                - { name: app, image: nginx }
"#;
        let output = inject(
            manifest,
            &"a".repeat(64),
            "owner",
            "podmesh/sidecar:latest",
            &test_proxy_endpoints(),
            &"r".repeat(32),
            &[],
        )
        .unwrap();
        let docs = protocol::manifest_yaml::parse_yaml_documents_from_slice(&output).unwrap();
        let value = serde_json::to_value(&docs[0]).unwrap();
        let generated_name = value["metadata"]["name"].as_str().unwrap();
        assert_eq!(format!("{generated_name}-pod").len(), MAX_DNS_LABEL_LEN);
        assert!(generated_name.starts_with(RUNTIME_NAME_PREFIX));
        let containers = value
            .pointer("/spec/template/spec/containers")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[1]["name"], SIDECAR_NAME);
    }

    #[test]
    fn runtime_name_depends_on_the_complete_workload_id() {
        let common_prefix = "a".repeat(63);
        let first = workload_runtime_name(&format!("{common_prefix}1"));
        let second = workload_runtime_name(&format!("{common_prefix}2"));

        assert_ne!(first, second);
        assert_eq!(first.len() + PODMAN_WORKLOAD_SUFFIX_LEN, MAX_DNS_LABEL_LEN);
        assert_eq!(second.len() + PODMAN_WORKLOAD_SUFFIX_LEN, MAX_DNS_LABEL_LEN);
    }

    #[test]
    fn preserves_non_workload_documents() {
        let manifest = br#"kind: ConfigMap
metadata:
  name: config
---
kind: Pod
metadata:
  name: demo
spec:
  containers:
    - name: app
      image: nginx
"#;
        let output = inject(
            manifest,
            &"a".repeat(64),
            "owner",
            "podmesh/sidecar:latest",
            &test_proxy_endpoints(),
            &"r".repeat(32),
            &[],
        )
        .unwrap();
        let docs = protocol::manifest_yaml::parse_yaml_documents_from_slice(&output).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(
            docs[0].get("kind").and_then(serde_yaml::Value::as_str),
            Some("ConfigMap")
        );
    }
}
