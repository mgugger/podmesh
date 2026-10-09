use anyhow::Context;
use clap::Subcommand;

const SECONDS_PER_DAY: u64 = 86_400;
/// Default grant lifetime. Must stay at or below
/// `MAX_PROXY_GRANT_LIFETIME_SECS`, otherwise the command cannot run at all.
pub const DEFAULT_PROXY_GRANT_TTL_DAYS: u64 = 30;

#[derive(Subcommand, Debug)]
pub enum CertCommands {
    /// Provision an owner-signed Biscuit grant to a trusted proxy.
    GrantProxy {
        /// Base URL of the proxy REST API (e.g. http://10.0.0.5:7100).
        #[arg(long)]
        proxy_url: String,
        /// Path to the operator's Ed25519 public key (raw 32 bytes).
        #[arg(long)]
        owner_pub: Option<String>,
        /// Path to the operator's Ed25519 private key (raw 32 bytes).
        #[arg(long)]
        owner_sk: Option<String>,
        /// Grant TTL in days.
        #[arg(long, default_value_t = DEFAULT_PROXY_GRANT_TTL_DAYS)]
        ttl_days: u64,
    },
    /// Observe and persist a proxy's signed identity.
    TrustProxy {
        #[arg(long)]
        proxy_url: String,
        /// Replace an existing binding after printing both identities.
        #[arg(long)]
        replace: bool,
    },
    /// List every explicitly trusted proxy identity.
    ListProxies,
    /// Remove a proxy binding. Removing an absent binding is successful.
    RemoveProxy {
        #[arg(long)]
        proxy_url: String,
    },
}

pub async fn handle_cert_command(cmd: CertCommands) -> anyhow::Result<()> {
    match cmd {
        CertCommands::GrantProxy {
            proxy_url,
            owner_pub,
            owner_sk,
            ttl_days,
        } => {
            let (owner_pk_bytes, owner_sk_bytes) = match (owner_pub, owner_sk) {
                (None, None) => crypto::load_or_create_signing_keypair(&crate::key_dir()?)
                    .context("load namespace signing key")?,
                (Some(public), Some(private)) => (
                    std::fs::read(&public)
                        .with_context(|| format!("reading owner_pub from {public}"))?,
                    std::fs::read(&private)
                        .with_context(|| format!("reading owner_sk from {private}"))?,
                ),
                _ => anyhow::bail!("pass both --owner-pub and --owner-sk, or neither"),
            };
            let ack =
                grant_proxy_async(&proxy_url, &owner_pk_bytes, &owner_sk_bytes, ttl_days).await?;
            println!("owner grant provisioned to proxy at {proxy_url}");
            println!("  owner_pubkey:        {}", ack.owner_pubkey);
            println!("  valid_until:         {}", ack.valid_until);
            println!("  message:             {}", ack.message);
        }
        CertCommands::TrustProxy { proxy_url, replace } => {
            let key_dir = crate::key_dir()?;
            let observed = observe_proxy_identity(&proxy_url).await?;
            println!("observed proxy identity:");
            print_proxy_identity(&observed);
            if replace {
                let old = crate::trust::list_proxy_trust(&key_dir)?
                    .into_iter()
                    .find(|entry| entry.origin == observed.origin)
                    .ok_or_else(|| {
                        crate::trust::TrustError::MissingProxyForReplace(
                            observed.origin.to_string(),
                        )
                    })?;
                println!("replacing trusted proxy identity:");
                print_proxy_identity(&old);
            }
            crate::trust::store_proxy_trust(&key_dir, observed.clone(), replace)?;
            println!("trusted proxy identity:");
            print_proxy_identity(&observed);
        }
        CertCommands::ListProxies => {
            for identity in crate::trust::list_proxy_trust(&crate::key_dir()?)? {
                print_proxy_identity(&identity);
            }
        }
        CertCommands::RemoveProxy { proxy_url } => {
            let removed = crate::trust::remove_proxy_trust(&crate::key_dir()?, &proxy_url)?;
            if let Some(identity) = removed {
                println!("removed trusted proxy identity:");
                print_proxy_identity(&identity);
            } else {
                println!(
                    "proxy {} was not trusted",
                    crate::trust::normalize_proxy_url(&proxy_url)?
                );
            }
        }
    }
    Ok(())
}

fn print_proxy_identity(identity: &crate::trust::TrustedProxyIdentity) {
    println!("  proxy_url:            {}", identity.origin);
    println!("  endpoint_id:          {}", identity.endpoint_id_hex);
    println!("  signing_pubkey:       {}", identity.signing_key_b64);
}

pub async fn observe_proxy_identity(
    proxy_url: &str,
) -> anyhow::Result<crate::trust::TrustedProxyIdentity> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(
        crate::bootstrap::fetch_proxy_identity(&reqwest::Client::new(), proxy_url, now)
            .await?
            .identity,
    )
}

/// Programmatic explicit trust operation. Calling this function is the trust
/// decision; ordinary apply and grant paths never call it.
pub async fn trust_proxy_async(
    proxy_url: &str,
    replace: bool,
) -> anyhow::Result<crate::trust::TrustedProxyIdentity> {
    trust_proxy_async_at(&crate::key_dir()?, proxy_url, replace).await
}

pub async fn trust_proxy_async_at(
    key_dir: &std::path::Path,
    proxy_url: &str,
    replace: bool,
) -> anyhow::Result<crate::trust::TrustedProxyIdentity> {
    let observed = observe_proxy_identity(proxy_url).await?;
    crate::trust::store_proxy_trust(key_dir, observed.clone(), replace)?;
    Ok(observed)
}

pub fn list_trusted_proxies() -> anyhow::Result<Vec<crate::trust::TrustedProxyIdentity>> {
    list_trusted_proxies_at(&crate::key_dir()?)
}

pub fn list_trusted_proxies_at(
    key_dir: &std::path::Path,
) -> anyhow::Result<Vec<crate::trust::TrustedProxyIdentity>> {
    crate::trust::list_proxy_trust(key_dir).map_err(Into::into)
}

pub fn remove_trusted_proxy(
    proxy_url: &str,
) -> anyhow::Result<Option<crate::trust::TrustedProxyIdentity>> {
    remove_trusted_proxy_at(&crate::key_dir()?, proxy_url)
}

pub fn remove_trusted_proxy_at(
    key_dir: &std::path::Path,
    proxy_url: &str,
) -> anyhow::Result<Option<crate::trust::TrustedProxyIdentity>> {
    crate::trust::remove_proxy_trust(key_dir, proxy_url).map_err(Into::into)
}

/// Result returned by [`grant_proxy`].
#[derive(Debug, Clone)]
pub struct GrantProxyResult {
    pub owner_pubkey: String,
    pub valid_until: u64,
    pub message: String,
}

/// Programmatic implementation of `podctl grant-proxy`. Suitable for both the
/// CLI and integration tests.
///
/// 1. Fetches and verifies the proxy's signed `EndpointRecord`.
/// 2. Requires an exact owner-local URL, endpoint-id, and signing-key binding.
/// 3. Mints a bounded Biscuit naming the trusted endpoint.
/// 4. POSTs the encoded grant to `<proxy_url>/api/v1/proxy_grant`.
pub fn grant_proxy(
    proxy_url: &str,
    owner_pub_bytes: &[u8],
    owner_sk_bytes: &[u8],
    ttl_days: u64,
) -> anyhow::Result<GrantProxyResult> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(grant_proxy_async(
        proxy_url,
        owner_pub_bytes,
        owner_sk_bytes,
        ttl_days,
    ))
}

/// Async variant of [`grant_proxy`]. Use this when calling from an existing
/// tokio runtime (such as integration tests).
pub async fn grant_proxy_async(
    proxy_url: &str,
    owner_pub_bytes: &[u8],
    owner_sk_bytes: &[u8],
    ttl_days: u64,
) -> anyhow::Result<GrantProxyResult> {
    grant_proxy_async_at(
        proxy_url,
        owner_pub_bytes,
        owner_sk_bytes,
        ttl_days,
        &crate::key_dir()?,
    )
    .await
}

pub async fn grant_proxy_async_at(
    proxy_url: &str,
    owner_pub_bytes: &[u8],
    owner_sk_bytes: &[u8],
    ttl_days: u64,
    key_dir: &std::path::Path,
) -> anyhow::Result<GrantProxyResult> {
    let client = reqwest::Client::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // A self-signed observation proves consistency, not authority. The owner
    // must already have bound this URL to the exact endpoint and signing key
    // before any owner-signed grant is minted.
    let observed = crate::bootstrap::fetch_proxy_identity(&client, proxy_url, now).await?;
    let trust = crate::trust::load_registry(key_dir)?;
    trust.authorize_proxy(&observed.identity)?;
    let peer_id = observed.identity.endpoint_id_hex;
    let base = observed.identity.origin.to_string();
    let lifetime = ttl_days
        .checked_mul(SECONDS_PER_DAY)
        .context("proxy grant lifetime overflowed")?;
    anyhow::ensure!(
        lifetime > 0 && lifetime <= protocol::MAX_PROXY_GRANT_LIFETIME_SECS,
        "proxy grant TTL must be between 1 day and {} days",
        protocol::MAX_PROXY_GRANT_LIFETIME_SECS / SECONDS_PER_DAY
    );
    let valid_until = now + lifetime;
    let owner_pub_b64 = crypto::b64_encode(owner_pub_bytes);
    let grant = protocol::mint_proxy_grant(
        owner_sk_bytes,
        owner_pub_bytes,
        &protocol::ProxyGrantClaims {
            tenant_owner: owner_pub_b64.clone(),
            proxy_endpoint: peer_id,
            issued_at_secs: now,
            expires_at_secs: valid_until,
            token_id: uuid::Uuid::new_v4().to_string(),
        },
        now,
    )?;

    let body = serde_json::json!({
        "owner_pubkey_b64": owner_pub_b64,
        "grant_b64": protocol::proxy_grant_to_b64(&grant),
    });

    let resp = client
        .post(format!("{}/api/v1/proxy_grant", base))
        .json(&body)
        .send()
        .await
        .context("POST /api/v1/proxy_grant failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("proxy rejected the owner grant: status={status} body={body_text}");
    }

    let ack: serde_json::Value = resp.json().await?;
    Ok(GrantProxyResult {
        owner_pubkey: ack
            .get("owner_pubkey")
            .and_then(|v| v.as_str())
            .unwrap_or(&owner_pub_b64)
            .to_string(),
        valid_until,
        message: ack
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    async fn untrusted_proxy_server() -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let task_requests = requests.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            task_requests.fetch_add(1, Ordering::SeqCst);
            let mut request = vec![0; 4096];
            let _ = stream.read(&mut request).await;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let (public, private) = crypto::generate_signing_keypair();
            let record = protocol::EndpointRecord {
                version: protocol::ENDPOINT_RECORD_VERSION,
                endpoint_id: vec![9; protocol::IROH_ENDPOINT_ID_BYTES],
                relay_url: None,
                direct_addresses: vec!["127.0.0.1:7101".to_string()],
                signing_pubkey: String::new(),
                issued_at_secs: now,
                expires_at_secs: now + 60,
                signature: String::new(),
            }
            .sign(&public, &private, now)
            .unwrap();
            let body = serde_json::json!({
                "endpoint_record_b64": crypto::b64_encode(&record.to_bytes(now).unwrap()),
                "peer_id": hex::encode(&record.endpoint_id),
                "signing_pubkey_b64": record.signing_pubkey,
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();

            if tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_ok()
            {
                task_requests.fetch_add(1, Ordering::SeqCst);
            }
        });
        (format!("http://{address}"), requests)
    }

    #[tokio::test]
    async fn missing_trust_refuses_before_signing_or_grant_post() {
        let key_dir = tempfile::tempdir().unwrap();
        let (url, requests) = untrusted_proxy_server().await;
        let error = grant_proxy_async_at(&url, &[1; 32], b"not-a-private-key", 30, key_dir.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("is not trusted"));
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(requests.load(Ordering::SeqCst), 1, "grant POST was sent");
    }
}
