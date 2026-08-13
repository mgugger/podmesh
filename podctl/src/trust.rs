//! Which agents this owner is willing to hand plaintext to.
//!
//! A `CapacityOffer` is self-signed: the key that validates the signature is
//! carried inside the offer, and the offer also names the KEM key every payload
//! is encrypted to. So whoever answers `GET /api/v1/agents/select` chooses the
//! recipient — and therefore chooses who can read the workload. Verifying the
//! offer proves only that it is internally consistent; it proves nothing about
//! *who* the agent is.
//!
//! The owner therefore keeps a list of agent signing keys it is prepared to
//! deploy to. Selection results are checked against that list before any
//! ciphertext is produced, which is what makes the scheduler a blind relay in
//! practice and not just by intent.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

/// File inside the key directory listing one base64 agent signing key per line.
pub const TRUSTED_AGENTS_FILE: &str = "trusted_agents";
/// Comma-separated base64 agent signing keys, overriding the file when set.
pub const TRUSTED_AGENTS_ENV_VAR: &str = "PODMESH_TRUSTED_AGENTS";
/// Bounds the store so a stray file cannot be loaded without limit.
pub const MAX_TRUSTED_AGENTS: usize = 1024;

/// The owner's decision about which agents may host its workloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTrust {
    /// Only these base64 signing keys are acceptable.
    Allowlist(BTreeSet<String>),
    /// Any agent the mesh offers is acceptable. Chosen deliberately by the
    /// operator; it means any scheduler can nominate itself as the recipient.
    AnyAgent,
}

impl AgentTrust {
    /// Load the trust store, or return `None` when the owner has not made a
    /// decision yet. Callers must not silently treat `None` as "trust anything".
    pub fn load(key_dir: &Path) -> Result<Option<Self>> {
        if let Ok(value) = std::env::var(TRUSTED_AGENTS_ENV_VAR) {
            let keys = parse_keys(value.split(','))?;
            return Ok(if keys.is_empty() {
                None
            } else {
                Some(Self::Allowlist(keys))
            });
        }
        let path = Self::path(key_dir);
        if !path.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("read trusted agent list {}", path.display()))?;
        let keys = parse_keys(contents.lines().map(|line| {
            // Allow `#` comments so an operator can annotate where a key came from.
            line.split('#').next().unwrap_or_default()
        }))?;
        Ok(if keys.is_empty() {
            None
        } else {
            Some(Self::Allowlist(keys))
        })
    }

    pub fn path(key_dir: &Path) -> PathBuf {
        key_dir.join(TRUSTED_AGENTS_FILE)
    }

    /// Refuse an agent this owner has not agreed to trust.
    pub fn authorize(&self, agent_signing_pubkey: &str) -> Result<()> {
        match self {
            Self::AnyAgent => Ok(()),
            Self::Allowlist(keys) => {
                ensure!(
                    keys.contains(agent_signing_pubkey),
                    "the mesh offered agent {agent_signing_pubkey}, which is not in \
                     {TRUSTED_AGENTS_FILE}; add its signing key or re-run with --trust-any-agent"
                );
                Ok(())
            }
        }
    }
}

/// Resolve the trust decision for one invocation.
///
/// When no allowlist exists the operator must say so explicitly. Defaulting to
/// "trust anything" would silently hand the scheduler the ability to read every
/// workload, which is exactly the property the design claims to deny it.
pub fn resolve(key_dir: &Path, trust_any_agent: bool) -> Result<AgentTrust> {
    match (AgentTrust::load(key_dir)?, trust_any_agent) {
        (Some(AgentTrust::Allowlist(keys)), false) => Ok(AgentTrust::Allowlist(keys)),
        (_, true) => {
            log::warn!(
                "--trust-any-agent: any agent the mesh offers may read this workload's plaintext"
            );
            Ok(AgentTrust::AnyAgent)
        }
        (Some(AgentTrust::AnyAgent), false) => Ok(AgentTrust::AnyAgent),
        (None, false) => bail!(
            "no trusted agents configured. List the base64 Ed25519 signing keys of the \
             agents you are willing to run workloads on in {}, set {TRUSTED_AGENTS_ENV_VAR}, \
             or pass --trust-any-agent to accept whichever agent the mesh offers",
            AgentTrust::path(key_dir).display()
        ),
    }
}

fn parse_keys<'a>(values: impl Iterator<Item = &'a str>) -> Result<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let decoded = crypto::b64_decode(value)
            .with_context(|| format!("decode trusted agent key {value:?}"))?;
        ensure!(
            decoded.len() == crypto::ED25519_PUBLIC_KEY_SIZE,
            "trusted agent key {value:?} must decode to {} bytes",
            crypto::ED25519_PUBLIC_KEY_SIZE
        );
        ensure!(
            keys.len() < MAX_TRUSTED_AGENTS,
            "more than {MAX_TRUSTED_AGENTS} trusted agent keys configured"
        );
        keys.insert(value.to_string());
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> String {
        crypto::b64_encode(&[byte; crypto::ED25519_PUBLIC_KEY_SIZE])
    }

    #[test]
    fn an_allowlist_admits_only_listed_agents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            AgentTrust::path(dir.path()),
            format!("{}  # lab agent\n\n{}\n", key(1), key(2)),
        )
        .unwrap();
        let trust = AgentTrust::load(dir.path()).unwrap().unwrap();
        trust.authorize(&key(1)).unwrap();
        trust.authorize(&key(2)).unwrap();
        assert!(trust.authorize(&key(3)).is_err());
    }

    #[test]
    fn missing_trust_store_is_an_error_unless_the_operator_opts_out() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve(dir.path(), false).unwrap_err();
        assert!(error.to_string().contains("no trusted agents configured"));
        assert_eq!(resolve(dir.path(), true).unwrap(), AgentTrust::AnyAgent);
    }

    #[test]
    fn malformed_keys_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(AgentTrust::path(dir.path()), "not-base64!!\n").unwrap();
        assert!(AgentTrust::load(dir.path()).is_err());

        std::fs::write(AgentTrust::path(dir.path()), crypto::b64_encode(b"short")).unwrap();
        assert!(AgentTrust::load(dir.path()).is_err());
    }
}
