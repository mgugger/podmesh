use anyhow::{Context, Result};
use base64::Engine;
use log::debug;
use serde_json::Value as JsonValue;

/// Convert JSON requests into flatbuffer payloads for direct communication with the machine
pub struct FlatbufferClient {
    pub base_url: String,
    client: reqwest::Client,
    /// CLI's private key for decryption (signing key private bytes)
    private_key: Vec<u8>,
    /// CLI's signing public key (base64)
    public_key: String,
    /// CLI's KEM public key bytes (raw) used to advertise to machines
    kem_public_key: Vec<u8>,
    /// CLI's KEM private key bytes (raw) used for decryption
    kem_private_key: Vec<u8>,
    /// Machine's public key for encryption
    machine_public_key: Option<Vec<u8>>,
}

impl FlatbufferClient {
    pub fn new(base_url: String) -> Result<Self> {
        // Use persistent keypairs from disk to match machine expectations
        crypto::ensure_pqc_init()?;
        let (public_key_bytes, private_key) = crypto::ensure_keypair_on_disk()?;
        let public_key = base64::engine::general_purpose::STANDARD.encode(&public_key_bytes);

        // Obtain or generate a KEM keypair for the CLI so we can advertise our KEM public key
        // to machines (used to encrypt responses back to the CLI).
        // Use the existing helper which supports ephemeral KEM mode via env var.
        let (kem_pub_bytes, kem_priv_bytes) = crypto::ensure_kem_keypair_on_disk()?;

        Ok(Self {
            base_url,
            client: reqwest::Client::new(),
            private_key,
            public_key,
            kem_public_key: kem_pub_bytes,
            kem_private_key: kem_priv_bytes,
            machine_public_key: None,
        })
    }

    /// Fetch the machine's KEM public key from the /api/v1/kem_pubkey endpoint for encryption
    pub async fn fetch_machine_public_key(&mut self) -> Result<()> {
        let url = format!("{}/api/v1/kem_pubkey", self.base_url);
        log::info!("Fetching machine KEM public key from: {}", url);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to connect to machine at {}", url))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read response body".to_string());
            anyhow::bail!(
                "Failed to fetch machine pubkey: {} - Response: {}",
                status,
                body
            );
        }

        let pubkey_b64 = resp
            .text()
            .await
            .context("Failed to get response text")?;

        if pubkey_b64.starts_with("ERROR:") {
            anyhow::bail!("Machine returned error: {}", pubkey_b64);
        }

        log::debug!("Received pubkey response: {}", pubkey_b64);

        let kem_pubkey_bytes = base64::engine::general_purpose::STANDARD
            .decode(&pubkey_b64)
            .context("Failed to decode public key")?;

        log::info!(
            "Successfully fetched machine KEM public key ({} bytes)",
            kem_pubkey_bytes.len()
        );
        self.machine_public_key = Some(kem_pubkey_bytes);
        Ok(())
    }

    /// Decrypt a response from the machine
    fn decrypt_from_machine(&self, envelope_bytes: &[u8]) -> Result<Vec<u8>> {
        // Parse the envelope first
        let envelope = protocol::machine::root_as_envelope(envelope_bytes)
            .map_err(|e| anyhow::anyhow!("Failed to parse envelope: {}", e))?;

        // Extract the payload from the envelope
        let payload_bytes = envelope.payload_vec();

        // If the payload is encrypted (starts with 0x02), decrypt it
        if !payload_bytes.is_empty() && payload_bytes[0] == 0x02 {
            crypto::decrypt_payload_from_recipient_blob(&payload_bytes, &self.kem_private_key)
        } else {
            // Payload is not encrypted, return as-is
            Ok(payload_bytes)
        }
    }

    #[allow(dead_code)]
    async fn send_unencrypted_request(
        &self,
        url: &str,
        payload: &[u8],
        payload_type: &str,
    ) -> Result<Vec<u8>> {
        let kem_pub_b64 = base64::engine::general_purpose::STANDARD.encode(&self.kem_public_key);
        let envelope = p2p::envelope::create_signed_envelope(
            payload,
            payload_type,
            &self.private_key,
            &self.public_key,
            Some("cli-client"),
            Some(&kem_pub_b64),
        )?;

        let resp = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header("x-peer-id", "cli-client") // Identify as CLI client
            .body(envelope)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "HTTP error {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
        }

        let response_bytes = resp.bytes().await?.to_vec();
        log::debug!(
            "send_unencrypted_request: response_bytes.len()={}",
            response_bytes.len()
        );

        Ok(response_bytes)
    }

    /// Send encrypted request to a specific node (end-to-end encryption with target node)
    pub async fn send_encrypted_request_to_node(
        &self,
        url: &str,
        payload: &[u8],
        payload_type: &str,
        target_node_kem_pubkey: &[u8],
    ) -> Result<Vec<u8>> {
        let kem_pub_b64 = base64::engine::general_purpose::STANDARD.encode(&self.kem_public_key);
        // Encrypt for the target node, not the bootstrap/machine
        let envelope = p2p::envelope::create_encrypted_signed_envelope(
            payload,
            payload_type,
            target_node_kem_pubkey,
            &self.private_key,
            &self.public_key,
            Some("cli-client"),
            Some(&kem_pub_b64),
        )?;

        let resp = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header("x-peer-id", "cli-client")
            .body(envelope)
            .send()
            .await?;

        let status = resp.status();

        if status.as_u16() == 405 {
            let body_text = resp.text().await?;
            return Ok(body_text.into_bytes());
        }

        if !status.is_success() {
            let body_text = resp.text().await?;
            anyhow::bail!("Request failed: {} {}", status, body_text);
        }

        let response_bytes = resp.bytes().await?;

        // Response might be encrypted with our KEM key, try to decrypt
        if response_bytes.len() > 100 {
            debug!(
                "send_encrypted_request_to_node: response length={}, attempting envelope parsing",
                response_bytes.len()
            );

            if let Ok(envelope) = protocol::machine::root_as_envelope(&response_bytes) {
                if let Some(payload) = envelope.payload() {
                    debug!(
                        "send_encrypted_request_to_node: Detected envelope response, attempting decryption"
                    );
                    debug!(
                        "send_encrypted_request_to_node: envelope payload length: {:?}",
                        payload.len()
                    );

                    match crypto::decrypt_payload_from_recipient_blob(payload, &self.kem_private_key) {
                        Ok(decrypted) => {
                            debug!(
                                "send_encrypted_request_to_node: Decryption successful, decrypted length: {}",
                                decrypted.len()
                            );
                            debug!(
                                "send_encrypted_request_to_node: first 50 bytes of decrypted: {:?}",
                                &decrypted[..decrypted.len().min(50)]
                            );
                            return Ok(decrypted);
                        }
                        Err(e) => {
                            debug!(
                                "send_encrypted_request_to_node: Decryption failed: {:?}, returning raw bytes",
                                e
                            );
                        }
                    }
                } else {
                    debug!(
                        "send_encrypted_request_to_node: Not an envelope ({}), server likely sent unencrypted FlatBuffer response",
                        response_bytes.len()
                    );
                }
            }
        } else {
            debug!(
                "send_encrypted_request_to_node: response_bytes.len()={}, skipping decryption",
                response_bytes.len()
            );
        }

        Ok(response_bytes.to_vec())
    }

    pub async fn send_encrypted_request(
        &self,
        url: &str,
        payload: &[u8],
        payload_type: &str,
    ) -> Result<Vec<u8>> {
        let kem_pub_b64 = base64::engine::general_purpose::STANDARD.encode(&self.kem_public_key);
        let envelope = if let Some(machine_pubkey) = &self.machine_public_key {
            p2p::envelope::create_encrypted_signed_envelope(
                payload,
                payload_type,
                machine_pubkey,
                &self.private_key,
                &self.public_key,
                Some("cli-client"),
                Some(&kem_pub_b64),
            )?
        } else {
            p2p::envelope::create_signed_envelope(
                payload,
                payload_type,
                &self.private_key,
                &self.public_key,
                Some("cli-client"),
                Some(&kem_pub_b64),
            )?
        };

        let resp = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header("x-peer-id", "cli-client") // Identify as CLI client
            .body(envelope)
            .send()
            .await?;

        let status = resp.status();

        // Treat 405 Method Not Allowed specially: some clients (flatbuffer clients)
        // POST envelope payloads and expect the server to return a body even when the
        // server doesn't consider the request a 'success' status. In that case we
        // return the response body bytes instead of treating it as an error.
        if status.as_u16() == 405 {
            let body_text = resp.text().await?;
            return Ok(body_text.into_bytes());
        }

        if !status.is_success() {
            let body_text = resp.text().await?;
            anyhow::bail!("Request failed: {} {}", status, body_text);
        }

        let response_bytes = resp.bytes().await?;

        // Try to decrypt if it looks like an encrypted envelope
        if response_bytes.len() > 100 && self.machine_public_key.is_some() {
            log::debug!(
                "send_encrypted_request: response length={}, machine_pubkey available, attempting envelope parsing",
                response_bytes.len()
            );

            // Try to parse as envelope first
            match protocol::machine::root_as_envelope(&response_bytes) {
                Ok(envelope) => {
                    log::debug!(
                        "send_encrypted_request: Detected envelope response, attempting decryption"
                    );
                    log::debug!(
                        "send_encrypted_request: envelope payload length: {:?}",
                        envelope.payload().map(|p| p.len())
                    );
                    // It's an envelope, try to decrypt
                    match self.decrypt_from_machine(&response_bytes) {
                        Ok(decrypted) => {
                            log::info!(
                                "send_encrypted_request: Decryption successful, decrypted length: {}",
                                decrypted.len()
                            );
                            log::debug!(
                                "send_encrypted_request: first 50 bytes of decrypted: {:?}",
                                &decrypted[..std::cmp::min(50, decrypted.len())]
                            );
                            Ok(decrypted)
                        }
                        Err(e) => {
                            log::error!(
                                "send_encrypted_request: Decryption failed: {:?}, returning raw bytes",
                                e
                            );
                            // Decryption failed, return raw bytes
                            Ok(response_bytes.to_vec())
                        }
                    }
                }
                Err(e) => {
                    log::debug!(
                        "send_encrypted_request: Not an envelope ({}), server likely sent unencrypted FlatBuffer response",
                        e
                    );
                    // Server sent unencrypted FlatBuffer response directly, return as-is
                    Ok(response_bytes.to_vec())
                }
            }
        } else {
            log::debug!(
                "send_encrypted_request: response_bytes.len()={}, machine_public_key.is_some()={}, skipping decryption",
                response_bytes.len(),
                self.machine_public_key.is_some()
            );
            Ok(response_bytes.to_vec())
        }
    }

    /// Get candidates using flatbuffer capacity request
    pub async fn get_candidates(&self, task_id: &str, replicas: usize) -> Result<Vec<String>> {
        let requested = std::cmp::max(1, replicas);
        log::debug!(
            "get_candidates: called with task_id={} replicas={}",
            task_id,
            requested
        );
        let url = format!(
            "{}/tasks/{}/candidates?replicas={}",
            self.base_url.trim_end_matches('/'),
            task_id,
            requested
        );
        log::debug!("get_candidates: requesting URL: {}", url);

        // Send empty payload for GET-like request
        log::debug!("get_candidates: sending request...");
        let response_bytes = self
            .send_encrypted_request(&url, &[], "candidates_request")
            .await?;

        log::info!(
            "get_candidates: received response, {} bytes",
            response_bytes.len()
        );

        // Try to parse as flatbuffer CandidatesResponse first
        log::debug!(
            "get_candidates: attempting to parse {} bytes as flatbuffer",
            response_bytes.len()
        );
        match protocol::machine::root_as_candidates_response(&response_bytes) {
            Ok(candidates_response) => {
                log::debug!(
                    "get_candidates: successfully parsed flatbuffer, ok={}",
                    candidates_response.ok()
                );
                if candidates_response.ok() {
                    let responders: Vec<String> = candidates_response
                        .candidates()
                        .iter()
                        .filter_map(|candidate| {
                            let peer_id = candidate.peer_id()?;
                            let public_key = candidate.public_key().unwrap_or("");
                            Some(format!("{}:{}", peer_id, public_key))
                        })
                        .collect();
                    log::debug!("get_candidates: found {} responders", responders.len());
                    Ok(responders)
                } else {
                    log::debug!("get_candidates: flatbuffer indicates error, returning empty");
                    Ok(vec![])
                }
            }
            Err(e) => {
                // Fallback to JSON parsing for compatibility
                log::warn!("Failed to parse candidates response as flatbuffer: {:?}", e);
                log::warn!(
                    "First 100 bytes of response: {:?}",
                    &response_bytes[..std::cmp::min(100, response_bytes.len())]
                );
                let response_str = String::from_utf8(response_bytes.clone())?;
                log::warn!(
                    "Response string length: {}, content: '{}'",
                    response_str.len(),
                    response_str
                );
                if response_str.trim().is_empty() {
                    log::warn!("Response is empty, returning empty candidates list");
                    return Ok(vec![]);
                }
                let response_json: JsonValue = serde_json::from_str(&response_str)?;
                let responders = response_json
                    .get("responders")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                let peers: Vec<String> = responders
                    .into_iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();

                Ok(peers)
            }
        }
    }

    pub async fn send_delete_request(&self, url: &str, body: &[u8]) -> Result<Vec<u8>> {
        let kem_pub_b64 = base64::engine::general_purpose::STANDARD.encode(&self.kem_public_key);
        let envelope = if let Some(machine_pubkey) = &self.machine_public_key {
            p2p::envelope::create_encrypted_signed_envelope(
                body,
                "delete_request",
                machine_pubkey,
                &self.private_key,
                &self.public_key,
                Some("cli-client"),
                Some(&kem_pub_b64),
            )?
        } else {
            p2p::envelope::create_signed_envelope(
                body,
                "delete_request",
                &self.private_key,
                &self.public_key,
                Some("cli-client"),
                Some(&kem_pub_b64),
            )?
        };

        let resp = self
            .client
            .delete(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header("x-peer-id", "cli-client") // Identify as CLI client
            .body(envelope)
            .send()
            .await?;

        let status = resp.status();

        if !status.is_success() {
            let body_text = resp.text().await?;
            anyhow::bail!("Delete request failed: {} {}", status, body_text);
        }

        let response_bytes = resp.bytes().await?;

        // Try to decrypt if it looks like an encrypted envelope
        if response_bytes.len() > 100 && self.machine_public_key.is_some() {
            // Try to parse as envelope first
            match protocol::machine::root_as_envelope(&response_bytes) {
                Ok(_envelope) => {
                    // It's an envelope, try to decrypt
                    match self.decrypt_from_machine(&response_bytes) {
                        Ok(decrypted) => {
                            log::info!(
                                "send_delete_request: Decryption successful, decrypted length: {}",
                                decrypted.len()
                            );
                            return Ok(decrypted);
                        }
                        Err(e) => {
                            log::warn!(
                                "send_delete_request: Decryption failed: {:?}, returning raw bytes",
                                e
                            );
                            // Decryption failed, return raw bytes
                            return Ok(response_bytes.to_vec());
                        }
                    }
                }
                Err(_) => {
                    log::debug!(
                        "send_delete_request: Not an envelope, server sent unencrypted response"
                    );
                    // Server sent unencrypted response directly, return as-is
                    return Ok(response_bytes.to_vec());
                }
            }
        }

        Ok(response_bytes.to_vec())
    }
}
