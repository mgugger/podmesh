//! Owner-rooted, endpoint-bound workload identities.
//!
//! Delegation is an explicit two-Biscuit chain, not holder-added attenuation
//! facts. The owner's token names a specific issuer and deployment scope; the
//! issuer's token binds that authority to transport and application keys. All
//! authoritative facts are queried with `trusting authority`.

use anyhow::{Context, Result, ensure};
use biscuit_auth::{
    AuthorizerBuilder, Biscuit,
    builder::{fact, int, string},
};
use serde::{Deserialize, Serialize};

use crate::{
    biscuit_keys::biscuit_keypair_from_ed25519,
    proxy_grant::{authorizer_limits, biscuit_root_from_owner},
};

pub const WORKLOAD_IDENTITY_VERSION: u16 = 1;
pub const MAX_IDENTITY_TOKEN_BYTES: usize = 4096;
pub const MAX_DELEGATION_BYTES: usize = 6 * 1024;
pub const MAX_WORKLOAD_IDENTITY_BYTES: usize = 12 * 1024;
pub const DEFAULT_IDENTITY_LIFETIME_SECS: u64 = 3600;
pub const MAX_IDENTITY_LIFETIME_SECS: u64 = 3600;
pub const IDENTITY_RENEWAL_WINDOW_SECS: u64 = 300;
pub const IDENTITY_CLOCK_SKEW_SECS: u64 = 60;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentScope {
    pub tenant_owner: String,
    pub manifest_id: String,
    pub workload_id: String,
    pub revision_id: String,
    pub issuer_public_key_b64: String,
    /// A new owner-issued token ID explicitly starts a new enrollment generation.
    pub token_id: String,
    pub issued_at_secs: u64,
    /// None is standing authority, not an implicitly renewed expired token.
    pub expires_at_secs: Option<u64>,
    pub max_child_lifetime_secs: u64,
}

impl EnrollmentScope {
    fn fields(&self) -> Vec<(&'static str, String)> {
        vec![
            ("owner", self.tenant_owner.clone()),
            ("manifest", self.manifest_id.clone()),
            ("workload", self.workload_id.clone()),
            ("revision", self.revision_id.clone()),
            ("issuer", self.issuer_public_key_b64.clone()),
            ("generation", self.token_id.clone()),
            ("issued", self.issued_at_secs.to_string()),
            (
                "expires",
                self.expires_at_secs
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "standing".into()),
            ),
            ("child_lifetime", self.max_child_lifetime_secs.to_string()),
        ]
    }

    fn validate(&self) -> Result<()> {
        public_key(&self.tenant_owner)?;
        public_key(&self.issuer_public_key_b64)?;
        for value in [&self.manifest_id, &self.workload_id, &self.revision_id] {
            hex_id(value)?;
        }
        ensure!(
            !self.token_id.is_empty() && self.token_id.len() <= 128 && self.token_id.is_ascii(),
            "invalid enrollment generation"
        );
        ensure!(
            (1..=MAX_IDENTITY_LIFETIME_SECS).contains(&self.max_child_lifetime_secs),
            "invalid delegated child lifetime"
        );
        ensure!(
            self.issued_at_secs <= i64::MAX as u64,
            "invalid delegation issue time"
        );
        if let Some(expires) = self.expires_at_secs {
            ensure!(
                expires >= self.issued_at_secs && expires <= i64::MAX as u64,
                "invalid delegation expiry"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentDelegation {
    pub version: u16,
    pub scope: EnrollmentScope,
    pub biscuit_b64: String,
}

impl EnrollmentDelegation {
    pub fn mint(owner_private: &[u8], scope: EnrollmentScope, now: u64) -> Result<Self> {
        scope.validate()?;
        ensure!(
            scope.tenant_owner
                == crypto::b64_encode(
                    &biscuit_keypair_from_ed25519(owner_private)?
                        .public()
                        .to_bytes()
                ),
            "delegation owner signing key mismatch"
        );
        let biscuit_b64 = mint_token(owner_private, "workload_enrollment", &scope.fields())?;
        let value = Self {
            version: WORKLOAD_IDENTITY_VERSION,
            scope,
            biscuit_b64,
        };
        value.verify(Some(now))?;
        value.to_bytes()?;
        Ok(value)
    }

    /// Structure/signature verification is allowed for persisted carrier state.
    /// Only live issuance/session verification can grant traffic authority.
    pub fn verify(&self, now: Option<u64>) -> Result<()> {
        ensure!(
            self.version == WORKLOAD_IDENTITY_VERSION,
            "unsupported enrollment delegation version"
        );
        self.scope.validate()?;
        verify_token(
            &self.biscuit_b64,
            &public_key(&self.scope.tenant_owner)?,
            "workload_enrollment",
            &self.scope.fields(),
            now,
        )?;
        if let Some(now) = now {
            ensure!(
                self.scope.issued_at_secs <= now.saturating_add(IDENTITY_CLOCK_SKEW_SECS),
                "delegation is not yet valid"
            );
            if let Some(expires) = self.scope.expires_at_secs {
                ensure!(now <= expires, "enrollment delegation expired");
            }
        }
        Ok(())
    }

    pub fn verify_scope(
        &self,
        owner: &str,
        manifest: &str,
        workload: &str,
        issuer: Option<&str>,
        now: Option<u64>,
    ) -> Result<()> {
        ensure!(
            self.scope.tenant_owner == owner
                && self.scope.manifest_id == manifest
                && self.scope.workload_id == workload,
            "enrollment delegation scope mismatch"
        );
        if let Some(issuer) = issuer {
            ensure!(
                self.scope.issuer_public_key_b64 == issuer,
                "enrollment issuer mismatch"
            );
        }
        self.verify(now)
    }

    pub fn digest(&self) -> Result<String> {
        Ok(blake3::hash(&self.to_bytes()?).to_hex().to_string())
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        encode(self, MAX_DELEGATION_BYTES)
    }
    pub fn to_b64(&self) -> Result<String> {
        Ok(crypto::b64_encode(&self.to_bytes()?))
    }
    pub fn from_b64(value: &str) -> Result<Self> {
        decode_b64(value, MAX_DELEGATION_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkloadIdentityClaims {
    pub delegation_digest: String,
    pub endpoint_id: String,
    pub signing_public_key_b64: String,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
}

impl WorkloadIdentityClaims {
    fn fields(&self) -> Vec<(&'static str, String)> {
        vec![
            ("delegation", self.delegation_digest.clone()),
            ("endpoint", self.endpoint_id.clone()),
            ("signer", self.signing_public_key_b64.clone()),
            ("issued", self.issued_at_secs.to_string()),
            ("expires", self.expires_at_secs.to_string()),
        ]
    }
    fn validate(&self) -> Result<()> {
        hex_id(&self.delegation_digest)?;
        hex_id(&self.endpoint_id)?;
        public_key(&self.signing_public_key_b64)?;
        ensure!(
            self.expires_at_secs > self.issued_at_secs && self.expires_at_secs <= i64::MAX as u64,
            "invalid identity validity window"
        );
        ensure!(
            self.expires_at_secs - self.issued_at_secs <= MAX_IDENTITY_LIFETIME_SECS,
            "identity lifetime exceeds limit"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkloadIdentity {
    pub version: u16,
    pub delegation: EnrollmentDelegation,
    pub claims: WorkloadIdentityClaims,
    pub biscuit_b64: String,
}

impl WorkloadIdentity {
    pub fn mint(
        issuer_private: &[u8],
        delegation: EnrollmentDelegation,
        endpoint_id: String,
        signer: String,
        now: u64,
        lifetime: u64,
    ) -> Result<Self> {
        delegation.verify(Some(now))?;
        ensure!(
            crypto::b64_encode(
                &biscuit_keypair_from_ed25519(issuer_private)?
                    .public()
                    .to_bytes()
            ) == delegation.scope.issuer_public_key_b64,
            "identity signing issuer mismatch"
        );
        ensure!(
            (1..=delegation.scope.max_child_lifetime_secs).contains(&lifetime),
            "child exceeds delegated lifetime"
        );
        let expires_at_secs = now
            .checked_add(lifetime)
            .context("identity expiry overflow")?;
        let expires_at_secs = delegation
            .scope
            .expires_at_secs
            .map(|end| end.min(expires_at_secs))
            .unwrap_or(expires_at_secs);
        let claims = WorkloadIdentityClaims {
            delegation_digest: delegation.digest()?,
            endpoint_id,
            signing_public_key_b64: signer,
            issued_at_secs: now,
            expires_at_secs,
        };
        claims.validate()?;
        let biscuit_b64 = mint_token(issuer_private, "workload_identity", &claims.fields())?;
        let value = Self {
            version: WORKLOAD_IDENTITY_VERSION,
            delegation,
            claims,
            biscuit_b64,
        };
        value.verify(Some(now))?;
        value.to_bytes()?;
        Ok(value)
    }

    pub fn verify(&self, now: Option<u64>) -> Result<()> {
        ensure!(
            self.version == WORKLOAD_IDENTITY_VERSION,
            "unsupported workload identity version"
        );
        self.delegation.verify(now)?;
        self.claims.validate()?;
        ensure!(
            self.claims.delegation_digest == self.delegation.digest()?,
            "identity delegation mismatch"
        );
        ensure!(
            self.claims.expires_at_secs - self.claims.issued_at_secs
                <= self.delegation.scope.max_child_lifetime_secs,
            "identity exceeds delegated lifetime"
        );
        ensure!(
            self.claims
                .issued_at_secs
                .saturating_add(IDENTITY_CLOCK_SKEW_SECS)
                >= self.delegation.scope.issued_at_secs,
            "identity predates delegation"
        );
        if let Some(end) = self.delegation.scope.expires_at_secs {
            ensure!(
                self.claims.expires_at_secs <= end,
                "identity outlives delegation"
            );
        }
        verify_token(
            &self.biscuit_b64,
            &public_key(&self.delegation.scope.issuer_public_key_b64)?,
            "workload_identity",
            &self.claims.fields(),
            now,
        )?;
        if let Some(now) = now {
            ensure!(
                self.claims.issued_at_secs <= now.saturating_add(IDENTITY_CLOCK_SKEW_SECS)
                    && now <= self.claims.expires_at_secs,
                "workload identity expired or not yet valid"
            );
        }
        Ok(())
    }

    pub fn verify_session(
        &self,
        owner: &str,
        manifest: &str,
        endpoint: &str,
        signing_key: &[u8],
        now: u64,
    ) -> Result<()> {
        ensure!(
            self.delegation.scope.tenant_owner == owner
                && self.delegation.scope.manifest_id == manifest,
            "workload identity tenant/scope mismatch"
        );
        ensure!(
            self.claims.endpoint_id == endpoint,
            "workload identity transport mismatch"
        );
        ensure!(
            self.claims.signing_public_key_b64 == crypto::b64_encode(signing_key),
            "workload identity application signer mismatch"
        );
        self.verify(Some(now))
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        encode(self, MAX_WORKLOAD_IDENTITY_BYTES)
    }
    pub fn to_b64(&self) -> Result<String> {
        Ok(crypto::b64_encode(&self.to_bytes()?))
    }
    pub fn from_b64(value: &str) -> Result<Self> {
        decode_b64(value, MAX_WORKLOAD_IDENTITY_BYTES)
    }
    pub fn from_bytes(value: &[u8]) -> Result<Self> {
        decode(value, MAX_WORKLOAD_IDENTITY_BYTES)
    }
}

fn mint_token(private: &[u8], role: &str, fields: &[(&str, String)]) -> Result<String> {
    let root = biscuit_keypair_from_ed25519(private)?;
    let mut builder = Biscuit::builder().fact(fact("role", &[string(role)]))?;
    for (key, value) in fields {
        builder = builder.fact(fact("claim", &[string(key), string(value)]))?;
    }
    let bytes = builder.build(&root)?.to_vec()?;
    ensure!(
        bytes.len() <= MAX_IDENTITY_TOKEN_BYTES,
        "identity Biscuit exceeds limit"
    );
    Ok(crypto::b64_encode(&bytes))
}

fn verify_token(
    encoded: &str,
    root: &[u8],
    role: &str,
    fields: &[(&str, String)],
    now: Option<u64>,
) -> Result<()> {
    ensure!(
        encoded.len() <= MAX_IDENTITY_TOKEN_BYTES.div_ceil(3) * 4,
        "identity Biscuit encoding exceeds limit"
    );
    let bytes = crypto::b64_decode(encoded)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_IDENTITY_TOKEN_BYTES,
        "invalid identity Biscuit size"
    );
    let token = Biscuit::from(&bytes, biscuit_root_from_owner(root)?)?;
    let mut builder = AuthorizerBuilder::new();
    let mut policy = format!("allow if role(\"{role}\")");
    for (index, (key, value)) in fields.iter().enumerate() {
        builder = builder.fact(fact("expected", &[string(key), string(value)]))?;
        policy.push_str(&format!(
            ", claim(\"{key}\", $v{index}), expected(\"{key}\", $v{index})"
        ));
    }
    // Authorizer facts remain trusted, but appended token blocks cannot assert
    // substitute authority facts or manufacture an issuer role.
    policy.push_str(" trusting authority; deny if true;");
    if let Some(now) = now {
        builder = builder.fact(fact("time", &[int(i64::try_from(now)?)]))?;
    }
    let mut authorizer = builder.code(&policy)?.build(&token)?;
    authorizer
        .authorize_with_limits(authorizer_limits())
        .context("identity Biscuit authorization failed")?;
    Ok(())
}

fn public_key(value: &str) -> Result<Vec<u8>> {
    ensure!(value.len() <= 44, "public key encoding exceeds limit");
    let key = crypto::b64_decode(value)?;
    ensure!(
        key.len() == 32 && crypto::b64_encode(&key) == value,
        "invalid identity public key"
    );
    Ok(key)
}
fn hex_id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid identity digest/endpoint"
    );
    Ok(())
}
fn encode(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= limit,
        "identity encoding exceeds limit"
    );
    Ok(bytes)
}
fn decode<T: serde::de::DeserializeOwned>(value: &[u8], limit: usize) -> Result<T> {
    ensure!(
        !value.is_empty() && value.len() <= limit,
        "identity encoding exceeds limit"
    );
    serde_json::from_slice(value).context("decode workload identity")
}
#[cfg(test)]
mod tests;

fn decode_b64<T: serde::de::DeserializeOwned>(value: &str, limit: usize) -> Result<T> {
    ensure!(
        value.len() <= limit.div_ceil(3) * 4,
        "identity base64 exceeds limit"
    );
    decode(&crypto::b64_decode(value)?, limit)
}
