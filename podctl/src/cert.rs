use anyhow::Context;
use clap::Subcommand;
use protocol::{NodeCert, NodeRole};
use std::str::FromStr;

#[derive(Subcommand, Debug)]
pub enum CertCommands {
    /// Issue a new NodeCert signed with the owner's key
    Issue {
        #[arg(long)]
        peer_id: String,
        /// Path to KEM public key file (raw bytes)
        #[arg(long)]
        kem_pub: String,
        /// Path to signing public key file (raw bytes)
        #[arg(long)]
        sign_pub: String,
        /// Path to signing private key file (raw bytes)
        #[arg(long)]
        sign_sk: String,
        #[arg(long, value_delimiter = ',')]
        caps: Vec<String>,
        #[arg(long, default_value = "both")]
        role: String,
        #[arg(long, default_value = "365")]
        valid_days: u64,
        /// Output path (default: node_cert.bin)
        #[arg(long)]
        output: Option<String>,
    },
    /// Show the contents of a NodeCert
    Show { path: String },
    /// Verify a NodeCert's owner signature
    Verify {
        cert_path: String,
        /// Path to owner public key file (raw bytes)
        #[arg(long)]
        owner_pub: String,
    },
    /// Provision a tenant-signed `NodeCert` to a proxy node so that sidecars
    /// belonging to the same tenant can discover and trust it.
    ///
    /// Fetches the proxy's signing pubkey, KEM pubkey and PeerId from its
    /// REST API, signs a `NodeRole::Proxy` `NodeCert` with the operator's
    /// Ed25519 key, and POSTs the encoded cert to the proxy.
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
        CertCommands::Issue {
            peer_id,
            kem_pub,
            sign_pub,
            sign_sk,
            caps,
            role,
            valid_days,
            output,
        } => {
            let kem_pub_bytes = std::fs::read(&kem_pub)
                .with_context(|| format!("reading kem_pub from {}", kem_pub))?;
            let sign_pub_bytes = std::fs::read(&sign_pub)
                .with_context(|| format!("reading sign_pub from {}", sign_pub))?;
            let sign_sk_bytes = std::fs::read(&sign_sk)
                .with_context(|| format!("reading sign_sk from {}", sign_sk))?;

            let node_role = NodeRole::from_str(&role)?;
            let valid_until = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + valid_days * 86400;

            let cert = NodeCert {
                peer_id,
                kem_pubkey: crypto::b64_encode(&kem_pub_bytes),
                signing_pubkey: crypto::b64_encode(&sign_pub_bytes),
                capabilities: caps,
                role: node_role,
                valid_until,
                owner_pubkey: crypto::b64_encode(&sign_pub_bytes),
                owner_sig: String::new(),
                endorsements: vec![],
            };

            let signed = cert.sign(&sign_sk_bytes, &sign_pub_bytes)?;
            let out_path = output.unwrap_or_else(|| "node_cert.bin".to_string());
            protocol::node_cert::save_node_cert(
                std::path::Path::new(&out_path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("."),
                &signed,
            )?;
            // save_node_cert writes to key_dir/node_cert.bin; if custom output path differs, rename
            let default_path = protocol::node_cert::default_node_cert_path(
                std::path::Path::new(&out_path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("."),
            );
            let target_path = std::path::PathBuf::from(&out_path);
            if default_path != target_path {
                std::fs::rename(&default_path, &target_path)?;
            }

            println!("NodeCert issued and saved to: {}", out_path);
            println!("  peer_id:    {}", signed.peer_id);
            println!("  role:       {}", signed.role);
            println!("  valid_until: {}", signed.valid_until);
            println!("  caps:       {:?}", signed.capabilities);
        }
        CertCommands::Show { path } => {
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading cert from {}", path))?;
            let cert = NodeCert::from_bytes(&bytes).with_context(|| "deserializing NodeCert")?;
            println!("NodeCert:");
            println!("  peer_id:       {}", cert.peer_id);
            println!("  role:          {}", cert.role);
            println!("  valid_until:   {}", cert.valid_until);
            println!("  expired:       {}", cert.is_expired());
            println!("  capabilities:  {:?}", cert.capabilities);
            println!("  kem_pubkey:    {}", cert.kem_pubkey);
            println!("  signing_pubkey:{}", cert.signing_pubkey);
            println!("  owner_pubkey:  {}", cert.owner_pubkey);
            println!("  owner_sig:     {}", cert.owner_sig);
            println!("  endorsements:  {}", cert.endorsements.len());
        }
        CertCommands::Verify {
            cert_path,
            owner_pub,
        } => {
            let bytes = std::fs::read(&cert_path)
                .with_context(|| format!("reading cert from {}", cert_path))?;
            let cert = NodeCert::from_bytes(&bytes).with_context(|| "deserializing NodeCert")?;

            // Override owner_pubkey from file for verification
            let owner_pk_bytes = std::fs::read(&owner_pub)
                .with_context(|| format!("reading owner_pub from {}", owner_pub))?;
            let mut check_cert = cert.clone();
            check_cert.owner_pubkey = crypto::b64_encode(&owner_pk_bytes);
            check_cert.verify()?;
            println!("NodeCert signature is valid.");
        }
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
            println!("NodeCert provisioned to proxy at {}", proxy_url);
            println!("  owner_pubkey:        {}", ack.owner_pubkey);
            println!("  tenant_dht_key_hex:  {}", ack.tenant_dht_key_hex);
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
    pub tenant_dht_key_hex: String,
    pub valid_until: u64,
    pub message: String,
}

/// Programmatic implementation of `podctl grant-proxy`. Suitable for both the
/// CLI and integration tests.
///
/// 1. Fetches the proxy's signing pubkey, KEM pubkey and PeerId from its REST API.
/// 2. Builds a `NodeCert` with `role = NodeRole::Proxy` and signs it with the
///    operator's Ed25519 key.
/// 3. POSTs the base64-postcard encoded cert to `<proxy_url>/api/v1/node_cert`.
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

    let signing_pubkey_b64: String = client
        .get(format!("{}/api/v1/signing_pubkey", base))
        .send()
        .await
        .context("GET /api/v1/signing_pubkey failed")?
        .error_for_status()
        .context("proxy returned error for signing_pubkey")?
        .json::<serde_json::Value>()
        .await?
        .get("pubkey_b64")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("signing_pubkey response missing pubkey_b64"))?;

    let kem_pubkey_b64: String = client
        .get(format!("{}/api/v1/kem_pubkey", base))
        .send()
        .await
        .context("GET /api/v1/kem_pubkey failed")?
        .error_for_status()
        .context("proxy returned error for kem_pubkey")?
        .json::<serde_json::Value>()
        .await?
        .get("pubkey_b64")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("kem_pubkey response missing pubkey_b64"))?;

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

    let valid_until = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + ttl_days * 86400;

    let owner_pub_b64 = crypto::b64_encode(owner_pub_bytes);
    let cert = NodeCert {
        peer_id,
        kem_pubkey: kem_pubkey_b64,
        signing_pubkey: signing_pubkey_b64,
        capabilities: vec!["proxy".to_string()],
        role: NodeRole::Proxy,
        valid_until,
        owner_pubkey: owner_pub_b64.clone(),
        owner_sig: String::new(),
        endorsements: vec![],
    };
    let signed = cert.sign(owner_sk_bytes, owner_pub_bytes)?;

    let body = serde_json::json!({
        "cert_b64": signed.to_b64(),
    });

    let resp = client
        .post(format!("{}/api/v1/node_cert", base))
        .json(&body)
        .send()
        .await
        .context("POST /api/v1/node_cert failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "proxy rejected NodeCert: status={} body={}",
            status,
            body_text
        );
    }

    let ack: serde_json::Value = resp.json().await?;
    Ok(GrantProxyResult {
        owner_pubkey: ack
            .get("owner_pubkey")
            .and_then(|v| v.as_str())
            .unwrap_or(&owner_pub_b64)
            .to_string(),
        tenant_dht_key_hex: ack
            .get("tenant_dht_key_hex")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        valid_until: ack
            .get("valid_until")
            .and_then(|v| v.as_u64())
            .unwrap_or(valid_until),
        message: ack
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

#[cfg(test)]
mod cert_tests {
    use super::*;
    use crypto::ensure_keypair_ephemeral;

    fn make_signed_cert(role: NodeRole) -> (NodeCert, Vec<u8>, Vec<u8>) {
        let (pk, sk) = ensure_keypair_ephemeral().unwrap();
        let (kem_pk, _) = ensure_keypair_ephemeral().unwrap();
        let cert = NodeCert {
            peer_id: "QmTest".to_string(),
            kem_pubkey: crypto::b64_encode(&kem_pk),
            signing_pubkey: crypto::b64_encode(&pk),
            capabilities: vec!["test".to_string()],
            role,
            valid_until: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 86400,
            owner_pubkey: crypto::b64_encode(&pk),
            owner_sig: String::new(),
            endorsements: vec![],
        };
        let signed = cert.sign(&sk, &pk).unwrap();
        (signed, sk, pk)
    }

    #[test]
    fn test_cert_issue_and_verify() {
        let (cert, _sk, _pk) = make_signed_cert(NodeRole::Proxy);
        assert!(cert.verify().is_ok());
    }

    #[test]
    fn test_cert_rejects_wrong_owner_key() {
        let (cert, _sk, _pk) = make_signed_cert(NodeRole::Proxy);
        let (wrong_pk, _) = ensure_keypair_ephemeral().unwrap();
        let mut tampered = cert.clone();
        tampered.owner_pubkey = crypto::b64_encode(&wrong_pk);
        assert!(tampered.verify().is_err());
    }

    #[test]
    fn test_proxy_role_is_distinct() {
        let (cert, _sk, _pk) = make_signed_cert(NodeRole::Proxy);
        assert!(cert.has_role(&NodeRole::Proxy));
    }
}
