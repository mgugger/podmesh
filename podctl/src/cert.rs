use anyhow::Context;
use clap::Subcommand;

const SECONDS_PER_DAY: u64 = 86_400;

#[derive(Subcommand, Debug)]
pub enum CertCommands {
    /// Provision an owner-signed Biscuit grant to a proxy so that sidecars
    /// belonging to the same tenant can verify and trust it.
    ///
    /// Fetches the proxy's EndpointId from its REST API, mints a Biscuit naming
    /// that endpoint with the namespace owner's Ed25519 key, and POSTs it to
    /// the proxy. The proxy can attenuate the grant further but never widen it.
    GrantProxy {
        /// Base URL of the proxy REST API (e.g. http://10.0.0.5:7100).
        #[arg(long)]
        proxy_url: String,
        /// Path to the operator's Ed25519 public key (raw 32 bytes).
        #[arg(long)]
        owner_pub: String,
        /// Path to the operator's Ed25519 private key (raw 32 bytes).
        #[arg(long)]
        owner_sk: String,
        /// Cert TTL in days.
        #[arg(long, default_value = "365")]
        ttl_days: u64,
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
            let owner_pk_bytes = std::fs::read(&owner_pub)
                .with_context(|| format!("reading owner_pub from {}", owner_pub))?;
            let owner_sk_bytes = std::fs::read(&owner_sk)
                .with_context(|| format!("reading owner_sk from {}", owner_sk))?;
            let ack =
                grant_proxy_async(&proxy_url, &owner_pk_bytes, &owner_sk_bytes, ttl_days).await?;
            println!("owner grant provisioned to proxy at {}", proxy_url);
            println!("  owner_pubkey:        {}", ack.owner_pubkey);
            println!("  valid_until:         {}", ack.valid_until);
            println!("  message:             {}", ack.message);
        }
    }
    Ok(())
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
/// 1. Fetches the proxy's EndpointId from `<proxy_url>/api/v1/peer_id`.
/// 2. Mints a Biscuit naming that endpoint, signed with the owner's Ed25519 key
///    and bounded by `MAX_PROXY_GRANT_LIFETIME_SECS`.
/// 3. POSTs the encoded grant to `<proxy_url>/api/v1/proxy_grant`.
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
    let client = reqwest::Client::new();
    let base = proxy_url.trim_end_matches('/');

    let peer_id: String = client
        .get(format!("{}/api/v1/peer_id", base))
        .send()
        .await
        .context("GET /api/v1/peer_id failed")?
        .error_for_status()
        .context("proxy returned error for peer_id")?
        .json::<serde_json::Value>()
        .await?
        .get("peer_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("peer_id response missing peer_id"))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
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
