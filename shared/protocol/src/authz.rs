use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use biscuit_auth::{
    AuthorizerBuilder, Biscuit, KeyPair, PrivateKey, PublicKey,
    builder::Algorithm,
};
use serde::{Deserialize, Serialize};

/// Canonical token schema id for the Biscuit rollout.
pub const AUTHZ_TOKEN_SCHEMA_V1: &str = "podmesh-biscuit-v1";

/// Authorization operation the caller is attempting.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthzOperation {
    Register,
    Ingress,
    Egress,
    ReleaseShare,
    DelegateShare,
}

/// Canonical ambient context passed into token verification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthzContext {
    pub tenant_owner_pubkey_b64: String,
    pub manifest_id: String,
    pub transport_peer_id: String,
    pub operation: AuthzOperation,
    pub http_path: Option<String>,
    pub dest_host: Option<String>,
    pub dest_port: Option<u16>,
    pub worker_peer_id: Option<String>,
    pub share_index: Option<u32>,
    pub delegate_peer_id: Option<String>,
    pub now_unix_secs: u64,
}

/// High-level decision consumed by callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthzDecision {
    Allow,
    Deny { reason: String },
}

impl AuthzDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Verifier abstraction so proxy/sidecar/custodian can share one call-path.
pub trait AuthzTokenVerifier: Send + Sync {
    fn verify_token(&self, token_bytes: &[u8], ctx: &AuthzContext) -> Result<()>;
}

/// Real Biscuit verifier for `release_share` operation.
///
/// This validates token signature chain with the provided root key and enforces
/// core release-share facts through authorizer checks.
pub struct BiscuitReleaseShareVerifier {
    pub root_public_key: PublicKey,
}

impl AuthzTokenVerifier for BiscuitReleaseShareVerifier {
    fn verify_token(&self, token_bytes: &[u8], ctx: &AuthzContext) -> Result<()> {
        if ctx.operation != AuthzOperation::ReleaseShare {
            anyhow::bail!("BiscuitReleaseShareVerifier only supports release_share operation");
        }

        let worker_peer = ctx
            .worker_peer_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("missing ctx.worker_peer_id for release_share verification"))?;

        let share_index = ctx
            .share_index
            .ok_or_else(|| anyhow!("missing ctx.share_index for release_share verification"))?;

        let biscuit = Biscuit::from(token_bytes, self.root_public_key)
            .context("biscuit signature verification failed")?;

        let code = format!(
            r#"
ctx_tenant("{tenant}");
ctx_manifest("{manifest}");
ctx_peer("{peer}");
ctx_worker("{worker}");
ctx_share_index({share_index});
ctx_now({now});

check if schema("{schema}");
check if operation("release_share");
check if tenant_owner($t), ctx_tenant($t);
check if manifest($m), ctx_manifest($m);
check if subject_peer($p), ctx_peer($p);
check if worker_peer($w), ctx_worker($w);
check if share_index($si), ctx_share_index($si);
check if issued_at($iat), ctx_now($n), $iat <= $n;
check if expires_at($exp), ctx_now($n), $n <= $exp;

allow if true;
"#,
            tenant = escape_biscuit_string(&ctx.tenant_owner_pubkey_b64),
            manifest = escape_biscuit_string(&ctx.manifest_id),
            peer = escape_biscuit_string(&ctx.transport_peer_id),
            worker = escape_biscuit_string(worker_peer),
            now = ctx.now_unix_secs,
            share_index = share_index,
            schema = AUTHZ_TOKEN_SCHEMA_V1,
        );

        let mut authorizer = AuthorizerBuilder::new()
            .code(code)
            .context("build authorizer code for release_share")?
            .build(&biscuit)
            .context("build authorizer from biscuit")?;

        authorizer
            .authorize()
            .context("biscuit release_share policy evaluation failed")?;

        Ok(())
    }
}

/// Mint a Biscuit token for release-share flows.
///
/// The token encodes canonical facts used by [`BiscuitReleaseShareVerifier`].
pub fn mint_release_share_token_b64(root_private_key: &[u8], ctx: &AuthzContext) -> Result<String> {
    if ctx.operation != AuthzOperation::ReleaseShare {
        anyhow::bail!("release_share token mint requires operation=release_share");
    }

    let worker_peer = ctx
        .worker_peer_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("worker_peer_id is required for release_share token mint"))?;

    let private_key = PrivateKey::from_bytes(root_private_key, Algorithm::Ed25519)
        .context("invalid root private key bytes for biscuit mint")?;
    let keypair = KeyPair::from(&private_key);

    let share_index = ctx
        .share_index
        .ok_or_else(|| anyhow!("share_index is required for release_share token mint"))?;

    let issued_at = ctx.now_unix_secs;
    let expires_at = issued_at.saturating_add(300);

    let token_code = format!(
        r#"
schema("{schema}");
tenant_owner("{tenant}");
manifest("{manifest}");
subject_peer("{peer}");
operation("release_share");
worker_peer("{worker}");
share_index({share_index});
issued_at({issued_at});
expires_at({expires_at});
token_id("{token_id}");
"#,
        schema = AUTHZ_TOKEN_SCHEMA_V1,
        tenant = escape_biscuit_string(&ctx.tenant_owner_pubkey_b64),
        manifest = escape_biscuit_string(&ctx.manifest_id),
        peer = escape_biscuit_string(&ctx.transport_peer_id),
        worker = escape_biscuit_string(worker_peer),
        share_index = share_index,
        issued_at = issued_at,
        expires_at = expires_at,
        token_id = escape_biscuit_string(&format!("{}:{}:{}", ctx.manifest_id, worker_peer, issued_at)),
    );

    let token = Biscuit::builder()
        .code(token_code)
        .context("build release_share token facts")?
        .build(&keypair)
        .context("build biscuit release_share token")?;

    let bytes = token.to_vec().context("serialize biscuit token")?;
    Ok(general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Default placeholder verifier kept for call-sites that are not migrated yet.
pub struct UnimplementedBiscuitVerifier;

impl AuthzTokenVerifier for UnimplementedBiscuitVerifier {
    fn verify_token(&self, _token_bytes: &[u8], _ctx: &AuthzContext) -> Result<()> {
        Err(anyhow!(
            "biscuit verifier backend not implemented for this operation (schema={})",
            AUTHZ_TOKEN_SCHEMA_V1
        ))
    }
}

/// Shared entrypoint for token verification decisions.
/// Missing/invalid/unverifiable token always denies.
pub fn verify_authz_token(
    token_b64: Option<&str>,
    ctx: &AuthzContext,
    verifier: &dyn AuthzTokenVerifier,
) -> AuthzDecision {
    let token_b64 = match token_b64 {
        Some(v) if !v.trim().is_empty() => v,
        _ => {
            let reason = format!(
                "missing authz token (operation={:?} manifest={} peer={})",
                ctx.operation, ctx.manifest_id, ctx.transport_peer_id
            );
            return AuthzDecision::Deny { reason };
        }
    };

    let token_bytes = match decode_token(token_b64) {
        Ok(b) => b,
        Err(err) => {
            return AuthzDecision::Deny {
                reason: format!("invalid authz token encoding: {err}"),
            }
        }
    };

    match verifier.verify_token(&token_bytes, ctx) {
        Ok(()) => AuthzDecision::Allow,
        Err(err) => AuthzDecision::Deny {
            reason: format!("authz verification failed: {err}"),
        },
    }
}

pub fn biscuit_public_key_from_ed25519_bytes(pubkey_bytes: &[u8]) -> Result<PublicKey> {
    PublicKey::from_bytes(pubkey_bytes, Algorithm::Ed25519)
        .context("invalid ed25519 public key for biscuit verifier")
}

fn decode_token(token_b64: &str) -> Result<Vec<u8>> {
    general_purpose::URL_SAFE_NO_PAD
        .decode(token_b64)
        .or_else(|_| general_purpose::URL_SAFE.decode(token_b64))
        .or_else(|_| general_purpose::STANDARD.decode(token_b64))
        .context("base64 decode failed")
}

fn escape_biscuit_string(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> AuthzContext {
        AuthzContext {
            tenant_owner_pubkey_b64: "owner".to_string(),
            manifest_id: "m1".to_string(),
            transport_peer_id: "p1".to_string(),
            operation: AuthzOperation::ReleaseShare,
            http_path: None,
            dest_host: None,
            dest_port: None,
            worker_peer_id: Some("p1".to_string()),
            share_index: Some(1),
            delegate_peer_id: None,
            now_unix_secs: 1_700_000_000,
        }
    }

    #[test]
    fn denies_missing_token() {
        let keypair = KeyPair::new();
        let verifier = BiscuitReleaseShareVerifier {
            root_public_key: keypair.public(),
        };
        let d = verify_authz_token(None, &ctx(), &verifier);
        assert!(matches!(d, AuthzDecision::Deny { .. }));
    }

    #[test]
    fn biscuit_roundtrip_allows_valid_release_share_token() {
        let keypair = KeyPair::new();
        let mut c = ctx();
        c.tenant_owner_pubkey_b64 = "tenant-a".into();

        let token = mint_release_share_token_b64(&keypair.private().to_bytes(), &c).unwrap();

        let verifier = BiscuitReleaseShareVerifier {
            root_public_key: keypair.public(),
        };
        let d = verify_authz_token(Some(&token), &c, &verifier);
        assert_eq!(d, AuthzDecision::Allow);
    }

    #[test]
    fn biscuit_denies_wrong_worker_binding() {
        let keypair = KeyPair::new();
        let c = ctx();
        let token = mint_release_share_token_b64(&keypair.private().to_bytes(), &c).unwrap();

        let verifier = BiscuitReleaseShareVerifier {
            root_public_key: keypair.public(),
        };

        let mut other = c.clone();
        other.worker_peer_id = Some("p2".into());
        let d = verify_authz_token(Some(&token), &other, &verifier);
        assert!(matches!(d, AuthzDecision::Deny { .. }));
    }
}
