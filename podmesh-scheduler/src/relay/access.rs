use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use iroh_relay::server::{Access, AccessControl, ClientRequest};
use protocol::MachineRelayGrant;

use super::MachineRelayConfig;
use crate::machine::IssuerRegistry;

#[derive(Debug, Clone)]
pub struct MachineRelayAccessControl {
    /// Trust converges as peer schedulers are discovered, so this is read per
    /// connection rather than frozen when the relay starts.
    trusted_issuers: IssuerRegistry,
    audience: String,
}

impl MachineRelayAccessControl {
    pub fn from_config(config: &MachineRelayConfig) -> Result<Self> {
        config.validate()?;
        let keys = config
            .trusted_issuer_keys
            .iter()
            .map(|key| crypto::b64_decode(key))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            keys.iter().all(|key| key.len() == 32),
            "relay trusted issuer key must contain 32 bytes"
        );
        Ok(Self {
            trusted_issuers: IssuerRegistry::new(keys)?,
            audience: config.canonical_audience()?,
        })
    }

    /// Handle onto the converging trust set, so peer discovery can extend it.
    pub fn issuers(&self) -> IssuerRegistry {
        self.trusted_issuers.clone()
    }

    fn authorize_token(&self, token: &str, endpoint_id: &[u8], now_secs: u64) -> Result<()> {
        let grant = MachineRelayGrant::from_auth_token(token, now_secs)?;
        grant.verify(
            &self.trusted_issuers.snapshot(),
            endpoint_id,
            &self.audience,
            now_secs,
        )
    }
}

impl AccessControl for MachineRelayAccessControl {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        let result = request
            .auth_token()
            .context("machine relay grant is missing")
            .and_then(|token| {
                self.authorize_token(&token, request.endpoint_id().as_bytes(), now_secs())
            });
        match result {
            Ok(()) => Access::Allow,
            Err(error) => {
                log::warn!(
                    "machine relay connection denied for endpoint {}: {error}",
                    request.endpoint_id().fmt_short()
                );
                Access::Deny {
                    reason: Some("invalid machine relay grant".into()),
                }
            }
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use protocol::{IROH_ENDPOINT_ID_BYTES, MACHINE_RELAY_GRANT_VERSION, MachineRole};

    use super::*;

    const NOW: u64 = 10_000;
    const AUDIENCE: &str = "https://relay.example.test";

    fn access_and_grant(role: MachineRole) -> (MachineRelayAccessControl, MachineRelayGrant) {
        let (issuer_public, issuer_private) = crypto::ensure_keypair_ephemeral().unwrap();
        let access = MachineRelayAccessControl {
            trusted_issuers: IssuerRegistry::new(vec![issuer_public.clone()]).unwrap(),
            audience: AUDIENCE.into(),
        };
        let grant = MachineRelayGrant {
            version: MACHINE_RELAY_GRANT_VERSION,
            subject_endpoint_id: vec![9; IROH_ENDPOINT_ID_BYTES],
            role,
            relay_audience: AUDIENCE.into(),
            issued_at_secs: NOW,
            expires_at_secs: NOW + 60,
            token_id: "grant-1".into(),
            issuer_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(&issuer_public, &issuer_private, NOW)
        .unwrap();
        (access, grant)
    }

    #[test]
    fn valid_machine_grant_is_admitted_without_retained_token_state() {
        let (access, grant) = access_and_grant(MachineRole::Scheduler);
        let token = grant.to_auth_token(NOW).unwrap();
        access
            .authorize_token(&token, &[9; IROH_ENDPOINT_ID_BYTES], NOW)
            .unwrap();
        access
            .authorize_token(&token, &[9; IROH_ENDPOINT_ID_BYTES], NOW)
            .unwrap();
    }

    #[test]
    fn wrong_subject_expiry_and_workload_roles_are_denied() {
        let (access, grant) = access_and_grant(MachineRole::Agent);
        let token = grant.to_auth_token(NOW).unwrap();
        assert!(
            access
                .authorize_token(&token, &[8; IROH_ENDPOINT_ID_BYTES], NOW)
                .is_err()
        );
        assert!(
            access
                .authorize_token(&token, &[9; IROH_ENDPOINT_ID_BYTES], NOW + 61)
                .is_err()
        );

        let (access, grant) = access_and_grant(MachineRole::Sidecar);
        assert!(
            access
                .authorize_token(
                    &grant.to_auth_token(NOW).unwrap(),
                    &[9; IROH_ENDPOINT_ID_BYTES],
                    NOW,
                )
                .is_err()
        );
    }
}
