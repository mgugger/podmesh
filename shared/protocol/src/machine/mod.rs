mod envelope;
mod manifest;
mod messages;
mod sidecar;
mod util;

pub use envelope::{
    Envelope, LegacyEnvelopeParts, SignedEnvelopeParams, build_encrypted_envelope,
    build_encrypted_envelope_with_peer, build_envelope_canonical,
    build_envelope_canonical_with_peer, build_envelope_signed, envelope_extract_sig_pub,
    envelope_extract_sig_pub_legacy, root_as_envelope,
};
pub use manifest::{
    AppliedManifest, KeyValue, OperationType, SignatureScheme, build_applied_manifest,
    root_as_applied_manifest,
};
pub use messages::Handshake;
pub use sidecar::{
    SidecarManifestRequest, SidecarProviderRecordOwned, SidecarProviderRecordParams,
    SidecarRouteKind, SidecarRouteSpec, build_sidecar_manifest_request,
    build_sidecar_provider_record, decode_sidecar_provider_record,
    root_as_sidecar_manifest_request, root_as_sidecar_provider_record,
};

pub fn build_handshake(
    nonce: u32,
    timestamp: u64,
    version: impl Into<String>,
    signature: impl Into<String>,
) -> Vec<u8> {
    build_handshake_with_cert(nonce, timestamp, version, signature, None)
}

pub fn build_handshake_with_cert(
    nonce: u32,
    timestamp: u64,
    version: impl Into<String>,
    signature: impl Into<String>,
    proxy_cert_b64: Option<&str>,
) -> Vec<u8> {
    Handshake {
        nonce,
        timestamp,
        protocol_version: version.into(),
        signature: signature.into(),
        proxy_cert_b64: proxy_cert_b64.unwrap_or_default().to_string(),
    }
    .to_bytes()
}

pub fn root_as_handshake(bytes: &[u8]) -> Result<Handshake, postcard::Error> {
    Handshake::from_bytes(bytes)
}
