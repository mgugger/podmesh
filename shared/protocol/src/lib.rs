pub mod node_cert;
pub use node_cert::{Endorsement, NodeCert, NodeRole};

pub mod sidecar_metadata;
pub mod libp2p_constants;
pub mod machine;
pub mod manifest_yaml;
pub mod manifest_policy;
pub mod egress;
pub mod podmesh_annotations;
pub use podmesh_annotations::PodmeshAnnotations;
pub mod sidecar_registration;
pub mod tenant_dht;
pub mod authz;

pub use authz::{
    AUTHZ_TOKEN_SCHEMA_V1, AuthzContext, AuthzDecision, AuthzOperation, AuthzTokenVerifier,
    BiscuitReleaseShareVerifier, UnimplementedBiscuitVerifier,
    biscuit_public_key_from_ed25519_bytes, mint_release_share_token_b64, verify_authz_token,
};
pub use sidecar_registration::{SidecarRegistration, SidecarRegistrationAck, SidecarRoute};
pub use tenant_dht::{
    compute_tenant_proxy_dht_key, compute_tenant_proxy_dht_key_from_bytes,
    compute_tenant_proxy_dht_key_hex,
};

#[cfg(test)]
mod tests {
    use super::machine::{
        build_envelope_canonical, build_envelope_signed, build_health, root_as_envelope,
        root_as_health,
    };

    #[test]
    fn postcard_health_roundtrip() {
        let bytes = build_health(true, "healthy");
        let health = root_as_health(&bytes).expect("health should deserialize");
        assert!(health.ok);
        assert_eq!(health.status(), Some("healthy"));
    }

    #[test]
    fn canonical_and_signed_envelope_share_payload() {
        let payload = b"demo";
        let canonical = build_envelope_canonical(payload, "demo", "nonce", 1, "ed25519", None);
        let signed = build_envelope_signed(
            payload,
            "demo",
            "nonce",
            1,
            "ed25519",
            "ed25519",
            "c2ln",
            "cHVi",
            None,
        );
        let canonical_env = root_as_envelope(&canonical).expect("canonical envelope");
        let signed_env = root_as_envelope(&signed).expect("signed envelope");
        assert_eq!(canonical_env.payload(), Some(&payload[..]));
        assert_eq!(canonical_env.payload(), signed_env.payload());
    }
}
