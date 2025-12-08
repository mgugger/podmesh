use anyhow::{Context, anyhow};
use protocol::machine::{SidecarProviderRecordOwned, decode_sidecar_provider_record};

use crate::envelope::{SignEnvelopeConfig, sign_with_node_keys};
use crate::security::verify_envelope_and_check_nonce;
use crate::util::timestamp_millis;

/// Payload type used for signed sidecar manifest envelopes.
pub const SIDECAR_MANIFEST_PAYLOAD_TYPE: &str = "sidecar-manifest-record";

/// Parsed information returned after verifying a manifest envelope.
pub struct VerifiedManifestRecord {
    pub record: SidecarProviderRecordOwned,
    pub signer_pubkey: Vec<u8>,
    pub timestamp_ms: u64,
}

/// Sign a serialized `SidecarProviderRecord` payload using the node's default keypair.
pub fn sign_sidecar_manifest_record(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let timestamp = timestamp_millis();
    let nonce = format!("sidecar_manifest_resp_{}", timestamp);
    let cfg = SignEnvelopeConfig {
        nonce: Some(&nonce),
        timestamp: Some(timestamp),
        ..Default::default()
    };
    Ok(sign_with_node_keys(payload, SIDECAR_MANIFEST_PAYLOAD_TYPE, cfg)?.bytes)
}

/// Verify a manifest envelope and return the decoded provider record plus signer metadata.
pub fn verify_sidecar_manifest_envelope(
    envelope_bytes: &[u8],
) -> anyhow::Result<VerifiedManifestRecord> {
    let parts = verify_envelope_and_check_nonce(envelope_bytes)?;
    if parts.payload_type != SIDECAR_MANIFEST_PAYLOAD_TYPE {
        return Err(anyhow!(
            "unexpected payload type {} for manifest envelope",
            parts.payload_type
        ));
    }

    let record = decode_sidecar_provider_record(&parts.payload)
        .context("failed to decode sidecar provider record payload")?;

    Ok(VerifiedManifestRecord {
        record,
        signer_pubkey: parts.pubkey,
        timestamp_ms: parts.timestamp_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::machine::{SidecarRouteKind, SidecarRouteSpec, build_sidecar_provider_record};

    #[test]
    fn manifest_envelope_roundtrip() {
        let original = crypto::get_keypair_config();
        let mut cfg = original.clone();
        cfg.signing_mode = crypto::KeypairMode::Ephemeral;
        cfg.kem_mode = crypto::KeypairMode::Ephemeral;
        crypto::set_keypair_config(cfg);

        let routes = vec![SidecarRouteSpec {
            host: "demo.mesh.local".to_string(),
            path_prefix: "/".to_string(),
            target_port: 8080,
            service_name: "svc".to_string(),
            service_port: "http".to_string(),
            source: SidecarRouteKind::Service,
        }];
        let payload = build_sidecar_provider_record(
            "demo",
            "peer-id",
            "demo.mesh.local",
            None,
            &routes,
            30_000,
            1234,
            1,
        );

        let envelope = sign_sidecar_manifest_record(&payload).expect("sign envelope");
        let verified = verify_sidecar_manifest_envelope(&envelope).expect("verify envelope");
        assert_eq!(verified.record.manifest_id, "demo");
        assert_eq!(verified.record.routes.len(), 1);

        crypto::set_keypair_config(original);
    }
}
