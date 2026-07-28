use anyhow::{Result, ensure};
use protocol::{
    AGENT_PROTOCOL_VERSION, AdmissionRequest, AgentControlOperation, DeploymentGrant,
    DeploymentReceipt, EncryptedWorkloadCapsule, ExecutionSpec, Reservation, WorkloadCommand,
    WorkloadCommandResponse, WorkloadOperation,
};
use rand::RngCore;

/// Carries an owner-encrypted control payload to the agent. Implemented once
/// for a direct Iroh dial and once for the scheduler's HTTP relay, so both
/// paths are proven to accept exactly the same owner-signed bytes.
pub trait ControlTransport {
    fn send(
        &self,
        operation: AgentControlOperation,
        encrypted_payload: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<Vec<u8>>> + Send;
}

pub async fn exercise_control_lifecycle<T: ControlTransport>(
    transport: &T,
    offer: &protocol::CapacityOffer,
) -> Result<()> {
    let agent_kem = crypto::b64_decode(&offer.kem_pubkey)?;
    let (owner_public, owner_private) = crypto::ensure_keypair_ephemeral()?;
    let (response_public, response_private) =
        crypto::keypair_manager::KeypairManager::generate_fresh_keypair(
            crypto::keypair_manager::KeypairType::Kem,
        )?;
    let namespace_id = crypto::b64_encode(&owner_public);
    let workload_id = protocol::workload_id(&owner_public, "iroh-control", 0);
    let admission = AdmissionRequest {
        version: AGENT_PROTOCOL_VERSION,
        request_id: "iroh-admission".into(),
        namespace_id: namespace_id.clone(),
        workload_id: workload_id.clone(),
        response_kem_pubkey: crypto::b64_encode(&response_public),
        cpu_milli: 500,
        memory_bytes: 512 * 1024 * 1024,
        storage_bytes: 4 * 1024 * 1024 * 1024,
        expires_at_secs: now_secs() + 30,
        nonce: "iroh-admission-nonce".into(),
        owner_signature: String::new(),
    }
    .sign(&owner_private)?;
    let reservation_body = transport
        .send(
            AgentControlOperation::Admission,
            encrypt_request(&agent_kem, &admission)?,
        )
        .await?;
    let reservation: Reservation = decrypt_response(&reservation_body, &response_private)?;
    ensure!(reservation.accepted, "agent rejected Iroh admission");

    let manifest = b"apiVersion: v1\nkind: Pod\nmetadata:\n  name: iroh-control\nspec:\n  containers:\n    - name: app\n      image: nginx\n".to_vec();
    let execution = ExecutionSpec {
        workload_name: "iroh-control".into(),
        replica_index: 0,
        replica_count: 1,
        manifest: manifest.clone(),
        proxy_endpoints: vec![test_proxy_endpoint()?],
        workload_relay_auth_token: "r".repeat(32),
        workload_relay_ca_certificates: Vec::new(),
    };
    let mut dek = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut dek);
    let (ciphertext, nonce) =
        crypto::encrypt_payload_with_key(&dek, &postcard::to_allocvec(&execution)?)?;
    let grant = DeploymentGrant {
        version: AGENT_PROTOCOL_VERSION,
        namespace_id: namespace_id.clone(),
        workload_id: workload_id.clone(),
        revision_id: protocol::revision_id(&manifest),
        target_node_id: offer.signing_pubkey.clone(),
        response_kem_pubkey: crypto::b64_encode(&response_public),
        reservation_id: reservation.reservation_id,
        capsule: EncryptedWorkloadCapsule {
            ciphertext,
            nonce: nonce.to_vec(),
            wrapped_dek: crypto::encrypt_payload_for_recipient(&agent_kem, &dek)?,
        },
        issued_at_secs: now_secs(),
        expires_at_secs: now_secs() + 30,
        nonce: "iroh-deploy-nonce".into(),
        owner_signature: String::new(),
    }
    .sign(&owner_private)?;
    let receipt_body = transport
        .send(
            AgentControlOperation::Deploy,
            encrypt_request(&agent_kem, &grant)?,
        )
        .await?;
    let receipt: DeploymentReceipt = decrypt_response(&receipt_body, &response_private)?;
    receipt.verify()?;

    for (operation, nonce) in [
        (WorkloadOperation::Status, "iroh-status"),
        (WorkloadOperation::Logs, "iroh-logs"),
        (WorkloadOperation::Delete, "iroh-delete"),
    ] {
        let command = WorkloadCommand {
            version: AGENT_PROTOCOL_VERSION,
            request_id: nonce.into(),
            namespace_id: namespace_id.clone(),
            workload_id: workload_id.clone(),
            operation,
            log_tail: Some(10),
            response_kem_pubkey: crypto::b64_encode(&response_public),
            expires_at_secs: now_secs() + 30,
            nonce: nonce.into(),
            owner_signature: String::new(),
        }
        .sign(&owner_private)?;
        let response_body = transport
            .send(
                AgentControlOperation::Command,
                encrypt_request(&agent_kem, &command)?,
            )
            .await?;
        let response: WorkloadCommandResponse =
            decrypt_response(&response_body, &response_private)?;
        response.verify()?;
        ensure!(response.ok, "Iroh lifecycle command failed");
    }

    Ok(())
}

fn test_proxy_endpoint() -> Result<protocol::EndpointRecord> {
    let now = now_secs();
    let (public, private) = crypto::ensure_keypair_ephemeral()?;
    protocol::EndpointRecord {
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
}

fn encrypt_request<T: serde::Serialize>(agent_kem: &[u8], value: &T) -> Result<Vec<u8>> {
    crypto::encrypt_payload_for_recipient(agent_kem, &postcard::to_allocvec(value)?)
}

fn decrypt_response<T: for<'de> serde::Deserialize<'de>>(
    body: &[u8],
    response_private: &[u8],
) -> Result<T> {
    Ok(postcard::from_bytes(
        &crypto::decrypt_payload_from_recipient_blob(body, response_private)?,
    )?)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
