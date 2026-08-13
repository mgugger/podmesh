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

/// Everything the injected sidecar needs to know about its workload.
pub struct SidecarInjection<'a> {
    pub workload_id: &'a str,
    pub workload_name: &'a str,
    /// Which replica this pod is, so the proxy can balance across siblings.
    pub replica_index: u32,
    pub replica_count: u32,
    /// Base64 Ed25519 key of the namespace owner.
    pub namespace_id: &'a str,
    pub sidecar_image: &'a str,
    pub proxy_endpoints: &'a [EndpointRecord],
    pub workload_credential_b64: &'a str,
    pub workload_relay_auth_token: &'a str,
    pub workload_relay_ca_certificates: &'a [Vec<u8>],
}

pub fn inject(manifest: &[u8], injection: SidecarInjection<'_>) -> Result<Vec<u8>> {
    let SidecarInjection {
        workload_id,
        workload_name,
        replica_index,
        replica_count,
        namespace_id,
        sidecar_image,
        proxy_endpoints,
        workload_credential_b64,
        workload_relay_auth_token,
        workload_relay_ca_certificates,
    } = injection;
    let original = manifest.to_vec();
    let owner = crypto::b64_decode(namespace_id).context("decode namespace owner key")?;
    let metadata = SidecarMetadata {
        workload_name: workload_name.to_string(),
        // The routing key is derived, not chosen, so the proxy can check that
        // whoever registers it holds the owner key it was derived from.
        manifest_id: protocol::route_id(&owner, workload_name),
        replica_index,
        replica_count,
        manifest_b64: crypto::b64_encode(&original),
        owner_public_key_b64: namespace_id.to_string(),
        proxy_endpoints: proxy_endpoints.to_vec(),
        workload_credential_b64: workload_credential_b64.to_string(),
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

    /// A real owner keypair, because the sidecar now carries a credential that
    /// only the owner's private key can mint.
    fn test_owner() -> (Vec<u8>, Vec<u8>) {
        crypto::generate_signing_keypair()
    }

    /// Credential the owner would have minted for this workload at deploy time.
    fn test_credential(owner_public: &[u8], owner_private: &[u8]) -> String {
        let now = crate::service::now_secs();
        let encoded = protocol::mint_workload_credential(
            owner_private,
            owner_public,
            &protocol::WorkloadCredentialClaims {
                tenant_owner: crypto::b64_encode(owner_public),
                manifest_id: protocol::route_id(owner_public, "demo"),
                issued_at_secs: now,
                expires_at_secs: now + 3600,
                token_id: "credential-1".into(),
            },
            now,
        )
        .unwrap();
        protocol::workload_credential_to_b64(&encoded)
    }

    fn test_injection<'a>(
        proxy_endpoints: &'a [EndpointRecord],
        owner: &'a str,
        credential: &'a str,
    ) -> SidecarInjection<'a> {
        SidecarInjection {
            workload_id: "a".repeat(64).leak(),
            workload_name: "demo",
            replica_index: 0,
            replica_count: 1,
            namespace_id: owner,
            sidecar_image: "podmesh/sidecar:latest",
            proxy_endpoints,
            workload_credential_b64: credential,
            workload_relay_auth_token: "r".repeat(32).leak(),
            workload_relay_ca_certificates: &[],
        }
    }

    fn test_proxy_endpoints() -> Vec<EndpointRecord> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (public, private) = crypto::generate_signing_keypair();
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
        let (owner_public, owner_private) = test_owner();
        let output = inject(
            manifest,
            test_injection(
                &test_proxy_endpoints(),
                &crypto::b64_encode(&owner_public),
                &test_credential(&owner_public, &owner_private),
            ),
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

    /// The spec promise is that the sidecar receives the tenant owner key, the
    /// proxy endpoints, the relay token and the relay CAs. Asserting on the
    /// decoded blob is the only way to know the promise actually holds.
    #[test]
    fn the_sidecar_receives_the_tenant_material() {
        let manifest = br#"apiVersion: v1
kind: Pod
metadata: { name: demo }
spec:
  containers:
    - { name: app, image: nginx }
"#;
        let (owner_public, owner_private) = test_owner();
        let owner = crypto::b64_encode(&owner_public);
        let credential = test_credential(&owner_public, &owner_private);
        let endpoints = test_proxy_endpoints();
        let output = inject(manifest, test_injection(&endpoints, &owner, &credential)).unwrap();

        let docs = protocol::manifest_yaml::parse_yaml_documents_from_slice(&output).unwrap();
        let value = serde_json::to_value(&docs[0]).unwrap();
        let sidecar = value
            .pointer("/spec/containers/1")
            .expect("sidecar container");
        assert_eq!(sidecar["name"], SIDECAR_NAME);

        let blob = sidecar["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == METADATA_BLOB_ENV_VAR)
            .and_then(|entry| entry["value"].as_str())
            .expect("metadata blob");
        let metadata: SidecarMetadata =
            serde_json::from_slice(&crypto::b64_decode(blob).unwrap()).unwrap();

        assert_eq!(metadata.owner_public_key_b64, owner);
        assert_eq!(metadata.workload_name, "demo");
        assert_eq!(
            metadata.manifest_id,
            protocol::route_id(&owner_public, "demo"),
            "the routing key must be derived from the owner key, not chosen"
        );
        assert_eq!(metadata.proxy_endpoints, endpoints);
        assert_eq!(metadata.workload_relay_auth_token, "r".repeat(32));
        assert_eq!(
            metadata.workload_credential_b64, credential,
            "the sidecar must receive the credential that proves its tenancy"
        );
        metadata
            .validate()
            .expect("injected metadata must validate");
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
        let (owner_public, owner_private) = test_owner();
        let output = inject(
            manifest,
            test_injection(
                &test_proxy_endpoints(),
                &crypto::b64_encode(&owner_public),
                &test_credential(&owner_public, &owner_private),
            ),
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
