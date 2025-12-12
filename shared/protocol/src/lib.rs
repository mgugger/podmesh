pub mod sidecar_metadata;
pub mod libp2p_constants;
pub mod machine;
pub mod manifest_yaml;
pub mod egress;

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
