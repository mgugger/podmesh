//! Explicit connector simulation for enrollment tests. No mocked identity is
//! created unless the test enables it for an already deployed workload.
use super::MockRuntime;
use anyhow::{Context, Result, ensure};
use protocol::{
    sidecar_metadata::{METADATA_BLOB_ENV_VAR, SidecarMetadata},
    workload_connector::{ConnectorIdentityReport, ConnectorRequest, ConnectorResponse},
};

pub(super) struct MockConnector {
    metadata: SidecarMetadata,
    identity: iroh_support::NodeIdentity,
    credential: Option<protocol::workload_identity::WorkloadIdentity>,
    sequence: u64,
}

impl MockConnector {
    pub(super) fn network_configured(&self) -> bool {
        self.metadata.service_catalog.is_some()
    }
}
impl MockRuntime {
    pub async fn enable_connector(&self, runtime_id: &str) -> Result<()> {
        let manifest = self
            .deployed_manifest(runtime_id)
            .await
            .context("mock workload not found")?;
        let documents = protocol::manifest_yaml::parse_yaml_documents_from_slice(&manifest)?;
        let mut metadata = None;
        for document in documents {
            let json = serde_json::to_value(document)?;
            let containers = json
                .pointer("/spec/containers")
                .or_else(|| json.pointer("/spec/template/spec/containers"));
            if let Some(containers) = containers.and_then(|value| value.as_array()) {
                for container in containers {
                    if container["name"] != crate::sidecar::SIDECAR_NAME {
                        continue;
                    }
                    let env = container["env"]
                        .as_array()
                        .context("mock connector environment missing")?;
                    let encoded = env
                        .iter()
                        .find(|entry| entry["name"] == METADATA_BLOB_ENV_VAR)
                        .and_then(|entry| entry["value"].as_str())
                        .context("mock connector metadata missing")?;
                    metadata = Some(serde_json::from_slice::<SidecarMetadata>(
                        &crypto::b64_decode(encoded)?,
                    )?);
                }
            }
        }
        let metadata = metadata.context("mock connector not injected")?;
        metadata.validate()?;
        self.connector_nodes.write().await.insert(
            runtime_id.into(),
            MockConnector {
                metadata,
                identity: iroh_support::NodeIdentity::ephemeral(),
                credential: None,
                sequence: 0,
            },
        );
        Ok(())
    }

    pub(super) async fn mock_connector_exchange(
        &self,
        runtime_id: &str,
        request: ConnectorRequest,
    ) -> Result<Option<ConnectorResponse>> {
        let mut nodes = self.connector_nodes.write().await;
        let Some(node) = nodes.get_mut(runtime_id) else {
            return Ok(None);
        };
        let now = crate::service::now_secs();
        if let Some(record) = &request.seed_peer {
            record.verify(&node.metadata.owner_public_key_b64, now)?;
        }
        if let Some(identity) = request.install_identity {
            let parent = protocol::workload_identity::EnrollmentDelegation::from_b64(
                &node.metadata.workload_credential_b64,
            )?;
            ensure!(
                identity.delegation == parent,
                "mock identity generation mismatch"
            );
            identity.verify_session(
                &node.metadata.owner_public_key_b64,
                &node.metadata.manifest_id,
                &node.identity.endpoint_id().to_string(),
                node.identity.signing_public(),
                now,
            )?;
            if let Some(old) = &node.credential {
                ensure!(
                    identity.claims.issued_at_secs >= old.claims.issued_at_secs
                        && identity.claims.expires_at_secs >= old.claims.expires_at_secs,
                    "mock renewal regression"
                );
            }
            node.credential = Some(identity);
        }
        let owner = crypto::b64_decode(&node.metadata.owner_public_key_b64)?;
        let endpoint = protocol::EndpointRecord {
            version: protocol::ENDPOINT_RECORD_VERSION,
            endpoint_id: node.identity.endpoint_id().as_bytes().to_vec(),
            relay_url: None,
            direct_addresses: vec!["127.0.0.1:1234".into()],
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: now + 300,
            signature: String::new(),
        }
        .sign(
            node.identity.signing_public(),
            node.identity.signing_private(),
            now,
        )?;
        let service_record = if let (Some(catalog), Some(credential)) =
            (&node.metadata.service_catalog, &node.credential)
        {
            node.sequence += 1;
            Some(
                protocol::service_discovery::ReplicaServiceRecord {
                    version: protocol::service_discovery::SERVICE_RECORD_VERSION,
                    catalog: catalog.clone(),
                    credential: credential.clone(),
                    endpoint: endpoint.clone(),
                    replica_index: node.metadata.replica_index,
                    sequence: node.sequence,
                    issued_at_secs: now,
                    expires_at_secs: (now + 120).min(credential.claims.expires_at_secs),
                    signature: String::new(),
                }
                .sign(node.identity.signing_private(), now)?,
            )
        } else {
            None
        };
        let report = ConnectorIdentityReport {
            owner_public_key_b64: node.metadata.owner_public_key_b64.clone(),
            workload_name: node.metadata.workload_name.clone(),
            workload_id: protocol::workload_id(
                &owner,
                &node.metadata.workload_name,
                node.metadata.replica_index,
            ),
            manifest_id: node.metadata.manifest_id.clone(),
            replica_index: node.metadata.replica_index,
            replica_count: node.metadata.replica_count,
            challenge: request.challenge,
            endpoint,
            credential_expires_at_secs: node
                .credential
                .as_ref()
                .map(|credential| credential.claims.expires_at_secs),
            credential: node.credential.clone(),
            service_record,
            transport_signature: String::new(),
        }
        .sign(&node.identity.transport_secret().to_bytes(), now)?;
        Ok(Some(ConnectorResponse::identity(report)))
    }
}
