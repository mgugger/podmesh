//! Bounded HTTP bootstrap helpers for proxy identity and relay configuration.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::de::DeserializeOwned;

use crate::trust::{CanonicalProxyOrigin, TrustedProxyIdentity};

pub(crate) const MAX_PROXY_BOOTSTRAP_RESPONSE_BYTES: usize = 64 * 1024;
pub(crate) const PROXY_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, serde::Deserialize)]
struct ProxyEndpointResponse {
    endpoint_record_b64: String,
    #[serde(default)]
    peer_id: Option<String>,
    #[serde(default)]
    signing_pubkey_b64: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProxyIdentityObservation {
    pub identity: TrustedProxyIdentity,
    pub endpoint_record_b64: String,
}

pub(crate) async fn send_bounded(
    request: reqwest::RequestBuilder,
) -> Result<(reqwest::StatusCode, Vec<u8>)> {
    tokio::time::timeout(PROXY_BOOTSTRAP_TIMEOUT, async {
		let mut response = request.send().await.context("send proxy bootstrap request")?;
		if let Some(length) = response.content_length() {
			ensure!(
				length <= MAX_PROXY_BOOTSTRAP_RESPONSE_BYTES as u64,
				"proxy bootstrap response exceeds the {MAX_PROXY_BOOTSTRAP_RESPONSE_BYTES} byte limit"
			);
		}
		let status = response.status();
		let mut body = Vec::new();
		while let Some(chunk) = response
			.chunk()
			.await
			.context("read proxy bootstrap response")?
		{
			ensure!(
				body.len().saturating_add(chunk.len()) <= MAX_PROXY_BOOTSTRAP_RESPONSE_BYTES,
				"proxy bootstrap response exceeds the {MAX_PROXY_BOOTSTRAP_RESPONSE_BYTES} byte limit"
			);
			body.extend_from_slice(&chunk);
		}
		Ok::<_, anyhow::Error>((status, body))
	})
	.await
	.context("proxy bootstrap request timed out")?
}

pub(crate) async fn bounded_json<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Result<(reqwest::StatusCode, T)> {
    let (status, body) = send_bounded(request).await?;
    let parsed = serde_json::from_slice(&body).context("decode proxy bootstrap response")?;
    Ok((status, parsed))
}

pub(crate) async fn fetch_proxy_identity(
    client: &reqwest::Client,
    proxy_url: &str,
    now_secs: u64,
) -> Result<ProxyIdentityObservation> {
    let origin = CanonicalProxyOrigin::parse(proxy_url)?;
    let (status, response): (_, ProxyEndpointResponse) =
        bounded_json(client.get(format!("{}/api/v1/endpoint_record", origin.as_str())))
            .await
            .with_context(|| format!("fetch proxy identity from {origin}"))?;
    ensure!(
        status.is_success(),
        "proxy {origin} returned status {status} for its endpoint record"
    );
    let encoded =
        crypto::b64_decode(&response.endpoint_record_b64).context("decode proxy EndpointRecord")?;
    let record = protocol::EndpointRecord::from_bytes(&encoded, now_secs)
        .context("verify proxy EndpointRecord")?;
    let endpoint_id_hex = hex::encode(&record.endpoint_id);
    if let Some(peer_id) = response.peer_id {
        ensure!(
            peer_id == endpoint_id_hex,
            "proxy endpoint response disagrees with its signed EndpointRecord"
        );
    }
    if let Some(signing_key) = response.signing_pubkey_b64 {
        ensure!(
            signing_key == record.signing_pubkey,
            "proxy signing-key response disagrees with its signed EndpointRecord"
        );
    }
    let identity = TrustedProxyIdentity::new(origin, &endpoint_id_hex, &record.signing_pubkey)?;
    Ok(ProxyIdentityObservation {
        identity,
        endpoint_record_b64: response.endpoint_record_b64,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn endpoint_response(peer_id: Option<String>) -> String {
        let now = now_secs();
        let (signing_public, signing_private) = crypto::generate_signing_keypair();
        let record = protocol::EndpointRecord {
            version: protocol::ENDPOINT_RECORD_VERSION,
            endpoint_id: vec![7; protocol::IROH_ENDPOINT_ID_BYTES],
            relay_url: None,
            direct_addresses: vec!["127.0.0.1:7101".to_string()],
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: now + 60,
            signature: String::new(),
        }
        .sign(&signing_public, &signing_private, now)
        .unwrap();
        serde_json::json!({
            "endpoint_record_b64": crypto::b64_encode(&record.to_bytes(now).unwrap()),
            "peer_id": peer_id.unwrap_or_else(|| hex::encode(&record.endpoint_id)),
            "signing_pubkey_b64": record.signing_pubkey,
        })
        .to_string()
    }

    async fn raw_server(response: Option<Vec<u8>>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let task_requests = requests.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            task_requests.fetch_add(1, Ordering::SeqCst);
            let mut request = vec![0; 4096];
            let _ = stream.read(&mut request).await;
            if let Some(response) = response {
                stream.write_all(&response).await.unwrap();
            } else {
                std::future::pending::<()>().await;
            }
        });
        (format!("http://{address}"), requests)
    }

    fn content_length_response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[tokio::test]
    async fn verified_identity_is_fetched_once() {
        let body = endpoint_response(None);
        let (url, requests) =
            raw_server(Some(content_length_response("200 OK", body.as_bytes()))).await;
        let observed = fetch_proxy_identity(&reqwest::Client::new(), &url, now_secs())
            .await
            .unwrap();
        assert_eq!(observed.identity.endpoint_id_hex, hex::encode([7u8; 32]));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn endpoint_response_identity_mismatch_is_refused() {
        let body = endpoint_response(Some(hex::encode([8u8; 32])));
        let (url, _) = raw_server(Some(content_length_response("200 OK", body.as_bytes()))).await;
        assert!(
            fetch_proxy_identity(&reqwest::Client::new(), &url, now_secs())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn oversized_content_length_is_refused_before_body_read() {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_PROXY_BOOTSTRAP_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let (url, _) = raw_server(Some(response)).await;
        let error = send_bounded(reqwest::Client::new().get(url))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("byte limit"));
    }

    #[tokio::test]
    async fn oversized_chunked_body_is_refused_incrementally() {
        let body = vec![b'x'; MAX_PROXY_BOOTSTRAP_RESPONSE_BYTES + 1];
        let mut response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        response.extend_from_slice(&body);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (url, _) = raw_server(Some(response)).await;
        let error = send_bounded(reqwest::Client::new().get(url))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("byte limit"));
    }

    #[tokio::test]
    async fn exact_limit_chunked_body_is_accepted_without_content_length() {
        let body = vec![b' '; MAX_PROXY_BOOTSTRAP_RESPONSE_BYTES - 4];
        let mut payload = b"null".to_vec();
        payload.extend_from_slice(&body);
        let mut response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(format!("{:x}\r\n", payload.len()).as_bytes());
        response.extend_from_slice(&payload);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (url, _) = raw_server(Some(response)).await;
        let (_, bytes) = send_bounded(reqwest::Client::new().get(url)).await.unwrap();
        assert_eq!(bytes.len(), MAX_PROXY_BOOTSTRAP_RESPONSE_BYTES);
    }

    #[tokio::test]
    async fn truncated_json_is_refused_after_bounded_read() {
        let (url, _) = raw_server(Some(content_length_response("200 OK", b"{\"value\":"))).await;
        let result: Result<(reqwest::StatusCode, serde_json::Value)> =
            bounded_json(reqwest::Client::new().get(url)).await;
        assert!(result.unwrap_err().to_string().contains("decode"));
    }

    #[tokio::test]
    async fn tampered_signed_endpoint_record_is_refused() {
        let body = endpoint_response(None);
        let mut value: serde_json::Value = serde_json::from_str(&body).unwrap();
        let encoded = value["endpoint_record_b64"].as_str().unwrap();
        let mut bytes = crypto::b64_decode(encoded).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        value["endpoint_record_b64"] = serde_json::Value::String(crypto::b64_encode(&bytes));
        let body = value.to_string();
        let (url, _) = raw_server(Some(content_length_response("200 OK", body.as_bytes()))).await;
        assert!(
            fetch_proxy_identity(&reqwest::Client::new(), &url, now_secs())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stalled_response_times_out_without_retry() {
        let (url, requests) = raw_server(None).await;
        let error = send_bounded(reqwest::Client::new().get(url))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
}
