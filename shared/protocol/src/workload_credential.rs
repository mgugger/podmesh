//! Owner-signed proof that a sidecar speaks for a tenant.
//!
//! A proxy must decide whether the endpoint talking to it really belongs to the
//! tenant it names. Nothing about the connection settles that on its own: a
//! sidecar's Iroh key is generated inside the container at start-up, so it is
//! unknown to the owner and proves only that the same peer keeps talking. The
//! owner's public key proves even less — it is public, so anyone can name it.
//!
//! Without this credential a caller can therefore claim any owner and, on a
//! proxy that owner has granted, register routes under that tenant's routing
//! key and receive its ingress traffic.
//!
//! `podctl` closes that by minting this credential at deploy time and sealing it
//! into the execution specification. It travels to the agent encrypted, is
//! injected into the sidecar's metadata, and is presented during the workload
//! handshake. Because the Biscuit's root key is the owner's, it cannot be minted
//! for a tenant whose private key the minter does not hold.
//!
//! It is a bearer credential: whoever reads a pod's metadata can act as that
//! workload. That is the same exposure as the relay token the metadata already
//! carries, narrowed from the whole mesh to one workload of one tenant. Binding
//! it to the sidecar's transport key is not possible, because that key does not
//! exist until long after the owner has gone away.

use anyhow::{Context, Result, ensure};
use biscuit_auth::{
    AuthorizerBuilder, Biscuit,
    builder::{fact, string},
};

use crate::biscuit_keys::{MAX_BISCUIT_TOKEN_BYTES, biscuit_keypair_from_ed25519};
use crate::proxy_grant::{authorizer_limits, biscuit_root_from_owner, int_term, validate_value};

/// Role asserted by a workload credential. A proxy refuses any other role, so a
/// proxy grant can never be replayed as a workload credential or the reverse.
pub const WORKLOAD_CREDENTIAL_ROLE: &str = "workload";
/// Operation asserted by a workload credential.
pub const WORKLOAD_CREDENTIAL_OPERATION: &str = "workload_session";
/// Longest lifetime an owner may mint. A pod may run for a long time, so this
/// matches the proxy grant bound rather than being shorter; re-deploying mints
/// a fresh one.
pub const MAX_WORKLOAD_CREDENTIAL_LIFETIME_SECS: u64 = 90 * 24 * 60 * 60;
/// Tolerated clock difference between the owner minting and the proxy verifying.
pub const MAX_WORKLOAD_CREDENTIAL_CLOCK_SKEW_SECS: u64 = 60;
/// Bounds the base64 form so an oversized token is rejected before decoding.
pub const MAX_WORKLOAD_CREDENTIAL_B64_LEN: usize = 4 * MAX_BISCUIT_TOKEN_BYTES.div_ceil(3);

/// What an owner asserts about one of its workloads.
#[derive(Debug, Clone)]
pub struct WorkloadCredentialClaims {
    /// Base64 Ed25519 public key of the namespace owner.
    pub tenant_owner: String,
    /// `route_id(owner_public_key, workload_name)`.
    ///
    /// Binds the credential to one workload, so a credential issued for one
    /// deployment cannot be used to claim another of the same tenant's routes.
    pub manifest_id: String,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    /// Unique identifier, so a specific credential can be told apart in logs.
    pub token_id: String,
}

impl WorkloadCredentialClaims {
    fn validate(&self, now_secs: u64) -> Result<()> {
        validate_value(&self.tenant_owner, "workload credential tenant owner")?;
        validate_value(&self.manifest_id, "workload credential manifest id")?;
        validate_value(&self.token_id, "workload credential token ID")?;
        ensure!(
            self.issued_at_secs <= now_secs.saturating_add(MAX_WORKLOAD_CREDENTIAL_CLOCK_SKEW_SECS),
            "workload credential issue time is too far in the future"
        );
        ensure!(
            self.expires_at_secs >= self.issued_at_secs,
            "workload credential expiry precedes issue time"
        );
        ensure!(
            self.expires_at_secs.saturating_sub(self.issued_at_secs)
                <= MAX_WORKLOAD_CREDENTIAL_LIFETIME_SECS,
            "workload credential lifetime exceeds {MAX_WORKLOAD_CREDENTIAL_LIFETIME_SECS} seconds"
        );
        Ok(())
    }
}

/// Mints an owner-signed credential for one workload.
///
/// `owner_private` and `owner_public` are the namespace owner's Ed25519 keys,
/// which only `podctl` holds.
pub fn mint_workload_credential(
    owner_private: &[u8],
    owner_public: &[u8],
    claims: &WorkloadCredentialClaims,
    now_secs: u64,
) -> Result<Vec<u8>> {
    claims.validate(now_secs)?;
    ensure!(
        claims.tenant_owner == crypto::b64_encode(owner_public),
        "workload credential tenant owner does not match the signing key"
    );
    let root = biscuit_keypair_from_ed25519(owner_private)?;
    let token = Biscuit::builder()
        .fact(fact("tenant_owner", &[string(&claims.tenant_owner)]))?
        .fact(fact("manifest", &[string(&claims.manifest_id)]))?
        .fact(fact("role", &[string(WORKLOAD_CREDENTIAL_ROLE)]))?
        .fact(fact("operation", &[string(WORKLOAD_CREDENTIAL_OPERATION)]))?
        .fact(fact(
            "issued_at",
            &[int_term(i64::try_from(claims.issued_at_secs)?)],
        ))?
        .fact(fact(
            "expires_at",
            &[int_term(i64::try_from(claims.expires_at_secs)?)],
        ))?
        .fact(fact("token_id", &[string(&claims.token_id)]))?
        .build(&root)
        .context("build workload credential Biscuit")?;
    let encoded = token
        .to_vec()
        .context("serialize workload credential Biscuit")?;
    ensure!(
        !encoded.is_empty() && encoded.len() <= MAX_BISCUIT_TOKEN_BYTES,
        "workload credential encoded size is invalid"
    );
    Ok(encoded)
}

/// Verifies a credential a sidecar presented.
///
/// The owner key is taken from what the caller claims, which is safe precisely
/// because it is also the Biscuit's root key: a credential only verifies if it
/// was signed by the private half. Claiming a tenant is therefore worthless
/// without that tenant's key, which is the property the caller relies on to
/// treat the named owner as proven.
pub fn verify_workload_credential(
    encoded: &[u8],
    claimed_tenant_owner: &str,
    claimed_manifest_id: &str,
    now_secs: u64,
) -> Result<()> {
    authorize(
        encoded,
        claimed_tenant_owner,
        claimed_manifest_id,
        Some(now_secs),
    )
}

/// Verifies everything except the validity window.
///
/// An agent re-reads a stored execution specification every time it reconciles
/// after a restart, and a pod may legitimately outlive the credential it was
/// deployed with. Enforcing expiry there would delete long-running workloads on
/// an unrelated restart, so expiry is enforced where the authorisation decision
/// is actually made: at the proxy. What matters here is that the credential is
/// genuinely the owner's and names this workload.
pub fn verify_workload_credential_structure(
    encoded: &[u8],
    claimed_tenant_owner: &str,
    claimed_manifest_id: &str,
) -> Result<()> {
    authorize(encoded, claimed_tenant_owner, claimed_manifest_id, None)
}

fn authorize(
    encoded: &[u8],
    claimed_tenant_owner: &str,
    claimed_manifest_id: &str,
    now_secs: Option<u64>,
) -> Result<()> {
    ensure!(
        !encoded.is_empty() && encoded.len() <= MAX_BISCUIT_TOKEN_BYTES,
        "workload credential encoded size is invalid"
    );
    validate_value(claimed_tenant_owner, "workload credential tenant owner")?;
    validate_value(claimed_manifest_id, "workload credential manifest id")?;
    let owner_public =
        crypto::b64_decode(claimed_tenant_owner).context("decode claimed tenant owner key")?;
    ensure!(
        owner_public.len() == crypto::ED25519_PUBLIC_KEY_SIZE,
        "claimed tenant owner key is not an Ed25519 public key"
    );
    let root_public = biscuit_root_from_owner(&owner_public)?;
    let token = Biscuit::from(encoded, root_public)
        .context("verify workload credential Biscuit signature")?;
    let builder = AuthorizerBuilder::new()
        .fact(fact(
            "request_tenant_owner",
            &[string(claimed_tenant_owner)],
        ))?
        .fact(fact("request_manifest", &[string(claimed_manifest_id)]))?;
    let builder = match now_secs {
        Some(now) => builder
            .fact(fact("request_time", &[int_term(i64::try_from(now)?)]))?
            .code(WORKLOAD_CREDENTIAL_POLICY)?,
        None => builder.code(WORKLOAD_CREDENTIAL_STRUCTURE_POLICY)?,
    };
    let mut authorizer = builder.build(&token)?;
    authorizer
        .authorize_with_limits(authorizer_limits())
        .context("workload credential authorization failed")?;
    Ok(())
}

pub fn workload_credential_to_b64(encoded: &[u8]) -> String {
    crypto::b64_encode(encoded)
}

pub fn workload_credential_from_b64(value: &str) -> Result<Vec<u8>> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_WORKLOAD_CREDENTIAL_B64_LEN,
        "workload credential length is invalid"
    );
    crypto::b64_decode(value)
}

/// Authority is granted only when the owner's facts match the request exactly
/// and the credential is inside its validity window. Any attenuation block only
/// narrows this further.
const WORKLOAD_CREDENTIAL_POLICY: &str = r#"
allow if tenant_owner($tenant), request_tenant_owner($tenant),
  manifest($manifest), request_manifest($manifest),
  role("workload"),
  operation("workload_session"),
  issued_at($issued), expires_at($expires), request_time($now),
  $issued <= $now, $now <= $expires,
  token_id($token_id);
deny if true;
"#;

/// The same policy with the validity window left out, for callers that must not
/// reject a credential merely because time has passed.
const WORKLOAD_CREDENTIAL_STRUCTURE_POLICY: &str = r#"
allow if tenant_owner($tenant), request_tenant_owner($tenant),
  manifest($manifest), request_manifest($manifest),
  role("workload"),
  operation("workload_session"),
  issued_at($issued), expires_at($expires),
  token_id($token_id);
deny if true;
"#;

#[cfg(test)]
#[path = "workload_credential/credential_tests.rs"]
mod credential_tests;
