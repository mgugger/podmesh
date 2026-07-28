//! Owner-signed proxy capabilities.
//!
//! `podctl` holds the namespace owner's Ed25519 keypair and is the only party
//! entitled to say which proxy may front that owner's workloads. It mints a
//! Biscuit naming a specific proxy endpoint and hands that token to the proxy.
//! A sidecar already receives the owner's public key inside its encrypted
//! metadata, so it can verify the grant a proxy presents during the workload
//! handshake without ever talking to `podctl`.
//!
//! A Biscuit is used rather than a bare signature because a Biscuit can be
//! attenuated offline: a proxy may append a block that narrows the grant it
//! holds — to a shorter lifetime, a subset of paths, or a single delegate —
//! and the result still verifies against the same owner key. Attenuation can
//! only ever remove authority, so a proxy cannot widen what the owner granted.

use anyhow::{Context, Result, ensure};
use biscuit_auth::{
    AuthorizerBuilder, Biscuit,
    builder::{Term, fact, string},
};

use crate::workload_authz::{
    MAX_AUTHZ_VALUE_LEN, MAX_BISCUIT_TOKEN_BYTES, biscuit_keypair_from_ed25519,
    biscuit_public_key_from_ed25519,
};

/// Role asserted by a proxy grant. A sidecar refuses any other role.
pub const PROXY_GRANT_ROLE: &str = "proxy";
/// Operation asserted by a proxy grant, keeping it unusable as an ingress,
/// egress or registration capability.
pub const PROXY_GRANT_OPERATION: &str = "proxy_service";
/// Longest lifetime an owner may hand a proxy. A grant is a standing authority,
/// so it is bounded to force periodic re-issue rather than living forever.
pub const MAX_PROXY_GRANT_LIFETIME_SECS: u64 = 90 * 24 * 60 * 60;
/// Tolerated clock difference between the owner minting a grant and the sidecar
/// verifying it.
pub const MAX_PROXY_GRANT_CLOCK_SKEW_SECS: u64 = 60;
/// Bounds the base64 form so an oversized token is rejected before decoding.
pub const MAX_PROXY_GRANT_B64_LEN: usize = 4 * MAX_BISCUIT_TOKEN_BYTES.div_ceil(3);

/// What an owner asserts about a proxy.
#[derive(Debug, Clone)]
pub struct ProxyGrantClaims {
    /// Base64 Ed25519 public key of the namespace owner.
    pub tenant_owner: String,
    /// Hex `EndpointId` of the proxy this grant names.
    pub proxy_endpoint: String,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    /// Unique identifier so a specific grant can be distinguished in logs.
    pub token_id: String,
}

impl ProxyGrantClaims {
    fn validate(&self, now_secs: u64) -> Result<()> {
        validate_value(&self.tenant_owner, "proxy grant tenant owner")?;
        validate_value(&self.proxy_endpoint, "proxy grant endpoint")?;
        validate_value(&self.token_id, "proxy grant token ID")?;
        ensure!(
            self.issued_at_secs <= now_secs.saturating_add(MAX_PROXY_GRANT_CLOCK_SKEW_SECS),
            "proxy grant issue time is too far in the future"
        );
        ensure!(
            self.expires_at_secs >= self.issued_at_secs,
            "proxy grant expiry precedes issue time"
        );
        ensure!(
            self.expires_at_secs.saturating_sub(self.issued_at_secs)
                <= MAX_PROXY_GRANT_LIFETIME_SECS,
            "proxy grant lifetime exceeds {MAX_PROXY_GRANT_LIFETIME_SECS} seconds"
        );
        Ok(())
    }
}

/// Mints an owner-signed grant for one proxy.
///
/// `owner_private` and `owner_public` are the namespace owner's Ed25519 keys,
/// which only `podctl` holds.
pub fn mint_proxy_grant(
    owner_private: &[u8],
    owner_public: &[u8],
    claims: &ProxyGrantClaims,
    now_secs: u64,
) -> Result<Vec<u8>> {
    claims.validate(now_secs)?;
    ensure!(
        claims.tenant_owner == crypto::b64_encode(owner_public),
        "proxy grant tenant owner does not match the signing key"
    );
    let root = biscuit_keypair_from_ed25519(owner_private)?;
    let token = Biscuit::builder()
        .fact(fact("tenant_owner", &[string(&claims.tenant_owner)]))?
        .fact(fact("subject_peer", &[string(&claims.proxy_endpoint)]))?
        .fact(fact("role", &[string(PROXY_GRANT_ROLE)]))?
        .fact(fact("operation", &[string(PROXY_GRANT_OPERATION)]))?
        .fact(fact(
            "issued_at",
            &[int_term(to_i64(claims.issued_at_secs)?)],
        ))?
        .fact(fact(
            "expires_at",
            &[int_term(to_i64(claims.expires_at_secs)?)],
        ))?
        .fact(fact("token_id", &[string(&claims.token_id)]))?
        .build(&root)
        .context("build proxy grant Biscuit")?;
    let encoded = token.to_vec().context("serialize proxy grant Biscuit")?;
    ensure!(
        !encoded.is_empty() && encoded.len() <= MAX_BISCUIT_TOKEN_BYTES,
        "proxy grant encoded size is invalid"
    );
    Ok(encoded)
}

/// Verifies a grant a proxy presented, binding it to the tenant the caller
/// belongs to and to the endpoint the connection actually came from.
///
/// `expected_proxy_endpoint` must be taken from the authenticated transport, not
/// from anything the peer claimed, so a stolen grant cannot be replayed by a
/// different endpoint.
pub fn verify_proxy_grant(
    encoded: &[u8],
    owner_public: &[u8],
    expected_tenant_owner: &str,
    expected_proxy_endpoint: &str,
    now_secs: u64,
) -> Result<()> {
    ensure!(
        !encoded.is_empty() && encoded.len() <= MAX_BISCUIT_TOKEN_BYTES,
        "proxy grant encoded size is invalid"
    );
    validate_value(expected_tenant_owner, "proxy grant tenant owner")?;
    validate_value(expected_proxy_endpoint, "proxy grant endpoint")?;
    ensure!(
        expected_tenant_owner == crypto::b64_encode(owner_public),
        "proxy grant tenant owner does not match the trusted owner key"
    );
    let root_public = biscuit_public_key_from_ed25519(owner_public)?;
    let token =
        Biscuit::from(encoded, root_public).context("verify proxy grant Biscuit signature")?;
    let mut authorizer = AuthorizerBuilder::new()
        .fact(fact(
            "request_tenant_owner",
            &[string(expected_tenant_owner)],
        ))?
        .fact(fact(
            "request_subject_peer",
            &[string(expected_proxy_endpoint)],
        ))?
        .fact(fact("request_time", &[int_term(to_i64(now_secs)?)]))?
        .code(PROXY_GRANT_POLICY)?
        .build(&token)?;
    authorizer
        .authorize()
        .context("proxy grant authorization failed")?;
    Ok(())
}

pub fn proxy_grant_to_b64(encoded: &[u8]) -> String {
    crypto::b64_encode(encoded)
}

pub fn proxy_grant_from_b64(value: &str) -> Result<Vec<u8>> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_PROXY_GRANT_B64_LEN,
        "proxy grant length is invalid"
    );
    crypto::b64_decode(value)
}

/// Authority is granted only when the owner's facts match the request exactly
/// and the grant is inside its validity window. Any attenuation block a proxy
/// appended is evaluated as an additional check, so it can only narrow this.
const PROXY_GRANT_POLICY: &str = r#"
allow if tenant_owner($tenant), request_tenant_owner($tenant),
  subject_peer($proxy), request_subject_peer($proxy),
  role("proxy"),
  operation("proxy_service"),
  issued_at($issued), expires_at($expires), request_time($now),
  $issued <= $now, $now <= $expires,
  token_id($token_id);
deny if true;
"#;

fn validate_value(value: &str, field: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_AUTHZ_VALUE_LEN,
        "{field} length is invalid"
    );
    Ok(())
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("proxy grant timestamp exceeds i64")
}

fn int_term(value: i64) -> Term {
    value.into()
}
