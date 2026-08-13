use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use iroh_relay::server::{Access, AccessControl, ClientRequest};
use protocol::MachineRelayGrant;

use super::MachineRelayConfig;
use crate::machine::{IssuerRegistry, MemberIssuers};

#[derive(Debug, Clone)]
pub struct MachineRelayAccessControl {
    /// Trust converges as peer schedulers are discovered, so this is read per
    /// connection rather than frozen when the relay starts.
    trusted_issuers: IssuerRegistry,
    /// Announced schedulers, each able to authorise only itself.
    member_issuers: MemberIssuers,
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
            member_issuers: MemberIssuers::new(),
            audience: config.canonical_audience()?,
        })
    }

    /// Handle onto the announced-member bindings, so gossip can extend them.
    pub fn member_issuers(&self) -> MemberIssuers {
        self.member_issuers.clone()
    }

    /// Handle onto the converging trust set, so peer discovery can extend it.
    pub fn issuers(&self) -> IssuerRegistry {
        self.trusted_issuers.clone()
    }

    fn authorize_token(&self, token: &str, endpoint_id: &[u8], now_secs: u64) -> Result<()> {
        let grant = MachineRelayGrant::from_auth_token(token, now_secs)?;
        let mut trusted = self.trusted_issuers.snapshot();
        // A pinned issuer may authorise anyone. An announced member may
        // authorise only the endpoint its announcement bound it to — and
        // `verify` separately requires that endpoint to be the one that
        // authenticated this connection, so it can only ever be itself. That is
        // what lets relay access scale with membership without turning every
        // member into an issuer that can hand this relay's bandwidth to
        // arbitrary third parties.
        if let Some(bound_endpoint) = self.member_issuers.endpoint_for(&grant.issuer_pubkey)
            && bound_endpoint == grant.subject_endpoint_id
        {
            trusted.push(crypto::b64_decode(&grant.issuer_pubkey)?);
        }
        grant.verify(&trusted, endpoint_id, &self.audience, now_secs)
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
        let (issuer_public, issuer_private) = crypto::generate_signing_keypair();
        let access = MachineRelayAccessControl {
            trusted_issuers: IssuerRegistry::new(vec![issuer_public.clone()]).unwrap(),
            member_issuers: MemberIssuers::new(),
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

    /// A scheduler admitted only through a gossip announcement, never pinned,
    /// still has to be able to reach a peer's relay — otherwise two schedulers
    /// behind NAT that know each other only by announcement can never meet.
    #[test]
    fn an_announced_member_may_authorise_its_own_endpoint() {
        let (member_public, member_private) = crypto::generate_signing_keypair();
        let access = MachineRelayAccessControl {
            // Deliberately empty: nothing here is pinned by an operator.
            trusted_issuers: IssuerRegistry::new(Vec::new()).unwrap(),
            member_issuers: MemberIssuers::new(),
            audience: AUDIENCE.into(),
        };
        access.member_issuers.bind(
            &crypto::b64_encode(&member_public),
            vec![9; IROH_ENDPOINT_ID_BYTES],
        );
        let grant = member_grant(
            vec![9; IROH_ENDPOINT_ID_BYTES],
            &member_public,
            &member_private,
        );
        access
            .authorize_token(
                &grant.to_auth_token(NOW).unwrap(),
                &[9; IROH_ENDPOINT_ID_BYTES],
                NOW,
            )
            .expect("an announced member must be able to use a peer relay for itself");
    }

    /// The restriction that keeps announcements from becoming a way to hand a
    /// relay's bandwidth to strangers: a member vouches for itself and nobody
    /// else.
    #[test]
    fn an_announced_member_cannot_authorise_another_endpoint() {
        let (member_public, member_private) = crypto::generate_signing_keypair();
        let access = MachineRelayAccessControl {
            trusted_issuers: IssuerRegistry::new(Vec::new()).unwrap(),
            member_issuers: MemberIssuers::new(),
            audience: AUDIENCE.into(),
        };
        access.member_issuers.bind(
            &crypto::b64_encode(&member_public),
            vec![9; IROH_ENDPOINT_ID_BYTES],
        );
        // The member mints a grant for somebody else's endpoint, and that
        // endpoint presents it.
        let grant = member_grant(
            vec![7; IROH_ENDPOINT_ID_BYTES],
            &member_public,
            &member_private,
        );
        assert!(
            access
                .authorize_token(
                    &grant.to_auth_token(NOW).unwrap(),
                    &[7; IROH_ENDPOINT_ID_BYTES],
                    NOW,
                )
                .is_err(),
            "an announced member must not be able to admit a third party"
        );
    }

    /// A key nobody announced is still worthless, so the binding is doing the
    /// work rather than the absence of pinning.
    #[test]
    fn an_unannounced_issuer_is_denied() {
        let (stranger_public, stranger_private) = crypto::generate_signing_keypair();
        let access = MachineRelayAccessControl {
            trusted_issuers: IssuerRegistry::new(Vec::new()).unwrap(),
            member_issuers: MemberIssuers::new(),
            audience: AUDIENCE.into(),
        };
        let grant = member_grant(
            vec![9; IROH_ENDPOINT_ID_BYTES],
            &stranger_public,
            &stranger_private,
        );
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

    fn member_grant(
        subject_endpoint_id: Vec<u8>,
        issuer_public: &[u8],
        issuer_private: &[u8],
    ) -> MachineRelayGrant {
        MachineRelayGrant {
            version: MACHINE_RELAY_GRANT_VERSION,
            subject_endpoint_id,
            role: MachineRole::Scheduler,
            relay_audience: AUDIENCE.into(),
            issued_at_secs: NOW,
            expires_at_secs: NOW + 60,
            token_id: "member-grant".into(),
            issuer_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(issuer_public, issuer_private, NOW)
        .unwrap()
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
