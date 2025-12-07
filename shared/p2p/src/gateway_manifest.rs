use anyhow::{Context, anyhow};
use protocol::machine::{GatewayProviderRecordOwned, decode_gateway_provider_record};

use crate::envelope::{SignEnvelopeConfig, sign_with_node_keys};
use crate::security::verify_envelope_and_check_nonce;
use crate::util::timestamp_millis;

/// Payload type used for signed gateway manifest envelopes.
pub const GATEWAY_MANIFEST_PAYLOAD_TYPE: &str = "gateway-manifest-record";

/// Parsed information returned after verifying a manifest envelope.
pub struct VerifiedManifestRecord {
    pub record: GatewayProviderRecordOwned,
    pub signer_pubkey: Vec<u8>,
    pub timestamp_ms: u64,
}

/// Sign a serialized `GatewayProviderRecord` payload using the node's default keypair.
pub fn sign_gateway_manifest_record(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let timestamp = timestamp_millis();
    let nonce = format!("gateway_manifest_resp_{}", timestamp);
    let cfg = SignEnvelopeConfig {
        nonce: Some(&nonce),
        timestamp: Some(timestamp),
        ..Default::default()
    };
    Ok(sign_with_node_keys(payload, GATEWAY_MANIFEST_PAYLOAD_TYPE, cfg)?.bytes)
}

/// Verify a manifest envelope and return the decoded provider record plus signer metadata.
pub fn verify_gateway_manifest_envelope(
    envelope_bytes: &[u8],
) -> anyhow::Result<VerifiedManifestRecord> {
    let parts = verify_envelope_and_check_nonce(envelope_bytes)?;
    if parts.payload_type != GATEWAY_MANIFEST_PAYLOAD_TYPE {
        return Err(anyhow!(
            "unexpected payload type {} for manifest envelope",
            parts.payload_type
        ));
    }

    let record = decode_gateway_provider_record(&parts.payload)
        .context("failed to decode gateway provider record payload")?;

    Ok(VerifiedManifestRecord {
        record,
        signer_pubkey: parts.pubkey,
        timestamp_ms: parts.timestamp_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::machine::{GatewayRouteKind, GatewayRouteSpec, build_gateway_provider_record};

    #[test]
    fn manifest_envelope_roundtrip() {
        let original = crypto::get_keypair_config();
        let mut cfg = original.clone();
        cfg.signing_mode = crypto::KeypairMode::Ephemeral;
        cfg.kem_mode = crypto::KeypairMode::Ephemeral;
        crypto::set_keypair_config(cfg);

        let routes = vec![GatewayRouteSpec {
            host: "demo.mesh.local".to_string(),
            path_prefix: "/".to_string(),
            target_port: 8080,
            service_name: "svc".to_string(),
            service_port: "http".to_string(),
            source: GatewayRouteKind::Service,
        }];
        let payload = build_gateway_provider_record(
            "demo",
            "peer-id",
            "demo.mesh.local",
            None,
            &routes,
            30_000,
            1234,
            1,
        );

        let envelope = sign_gateway_manifest_record(&payload).expect("sign envelope");
        let verified = verify_gateway_manifest_envelope(&envelope).expect("verify envelope");
        assert_eq!(verified.record.manifest_id, "demo");
        assert_eq!(verified.record.routes.len(), 1);

        crypto::set_keypair_config(original);
    }
}
