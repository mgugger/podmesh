//! Typed workload control-message sealing and acceptance.

use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    EndpointRecord, WorkloadStreamKind,
    machine::{
        ENVELOPE_SIGNATURE_ALGORITHM, ENVELOPE_VERSION, Envelope, EnvelopeParts,
        MAX_ENVELOPE_DRIFT_MS,
    },
    replay_registry::PeerReplayRegistry,
    workload_stream::{
        MAX_EGRESS_CONTROL_PAYLOAD_BYTES, MAX_HANDSHAKE_PAYLOAD_BYTES, MAX_INGRESS_PAYLOAD_BYTES,
        MAX_PROXY_ANNOUNCEMENT_PAYLOAD_BYTES, MAX_PROXY_DISCOVERY_PAYLOAD_BYTES,
        MAX_REGISTRATION_PAYLOAD_BYTES, WORKLOAD_ENVELOPE_OVERHEAD_BYTES,
    },
};

const ENDPOINT_ID_LEN: usize = 64;

pub trait WorkloadPayload: Serialize + DeserializeOwned {
    const TYPE: WorkloadPayloadType;

    fn validate(&self, now_secs: u64) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorkloadStreamPhase {
    #[default]
    AwaitingControlEnvelope,
    ControlAccepted,
    OperationAuthorized,
    Raw,
    Complete,
}

impl WorkloadStreamPhase {
    pub fn accept_control(&mut self) -> Result<()> {
        ensure!(
            *self == Self::AwaitingControlEnvelope,
            "control envelope is not expected in this stream phase"
        );
        *self = Self::ControlAccepted;
        Ok(())
    }

    pub fn authorize(&mut self) -> Result<()> {
        ensure!(
            *self == Self::ControlAccepted,
            "operation authorization requires accepted control"
        );
        *self = Self::OperationAuthorized;
        Ok(())
    }

    pub fn enter_raw(&mut self) -> Result<()> {
        ensure!(
            *self == Self::OperationAuthorized,
            "raw stream requires authorized control"
        );
        *self = Self::Raw;
        Ok(())
    }

    pub fn enter_websocket(
        &mut self,
        upgrade_requested: bool,
        status_code: u16,
        upgrade_accepted: bool,
    ) -> Result<()> {
        ensure!(
            upgrade_requested && upgrade_accepted && status_code == 101,
            "WebSocket raw mode requires an accepted status 101 transition"
        );
        self.enter_raw()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WorkloadPayloadType {
    HandshakeRequest,
    HandshakeResponse,
    RegistrationRequest,
    RegistrationResponse,
    ProxyDiscoveryRequest,
    ProxyDiscoveryResponse,
    IngressRequest,
    IngressResponse,
    EgressRequest,
    EgressResponse,
    ProxyAnnouncementRequest,
    ProxyAnnouncementResponse,
}

impl WorkloadPayloadType {
    pub const ALL: [Self; 12] = [
        Self::HandshakeRequest,
        Self::HandshakeResponse,
        Self::RegistrationRequest,
        Self::RegistrationResponse,
        Self::ProxyDiscoveryRequest,
        Self::ProxyDiscoveryResponse,
        Self::IngressRequest,
        Self::IngressResponse,
        Self::EgressRequest,
        Self::EgressResponse,
        Self::ProxyAnnouncementRequest,
        Self::ProxyAnnouncementResponse,
    ];

    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::HandshakeRequest => "podmesh.workload.handshake.request.v1",
            Self::HandshakeResponse => "podmesh.workload.handshake.response.v1",
            Self::RegistrationRequest => "podmesh.workload.registration.request.v1",
            Self::RegistrationResponse => "podmesh.workload.registration.response.v1",
            Self::ProxyDiscoveryRequest => "podmesh.workload.discovery.request.v1",
            Self::ProxyDiscoveryResponse => "podmesh.workload.discovery.response.v1",
            Self::IngressRequest => "podmesh.workload.ingress.request.v1",
            Self::IngressResponse => "podmesh.workload.ingress.response.v1",
            Self::EgressRequest => "podmesh.workload.egress.request.v1",
            Self::EgressResponse => "podmesh.workload.egress.response.v1",
            Self::ProxyAnnouncementRequest => "podmesh.workload.announcement.request.v1",
            Self::ProxyAnnouncementResponse => "podmesh.workload.announcement.response.v1",
        }
    }

    pub fn from_wire_name(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|payload_type| payload_type.wire_name() == value)
    }

    pub const fn frame_kind(self) -> WorkloadStreamKind {
        match self {
            Self::HandshakeRequest | Self::HandshakeResponse => WorkloadStreamKind::Handshake,
            Self::RegistrationRequest | Self::RegistrationResponse => {
                WorkloadStreamKind::Registration
            }
            Self::ProxyDiscoveryRequest | Self::ProxyDiscoveryResponse => {
                WorkloadStreamKind::ProxyDiscovery
            }
            Self::IngressRequest | Self::IngressResponse => WorkloadStreamKind::Ingress,
            Self::EgressRequest | Self::EgressResponse => WorkloadStreamKind::Egress,
            Self::ProxyAnnouncementRequest | Self::ProxyAnnouncementResponse => {
                WorkloadStreamKind::ProxyAnnouncement
            }
        }
    }

    pub const fn payload_limit(self) -> usize {
        match self.frame_kind() {
            WorkloadStreamKind::Handshake => MAX_HANDSHAKE_PAYLOAD_BYTES,
            WorkloadStreamKind::Registration => MAX_REGISTRATION_PAYLOAD_BYTES,
            WorkloadStreamKind::ProxyDiscovery => MAX_PROXY_DISCOVERY_PAYLOAD_BYTES,
            WorkloadStreamKind::Ingress => MAX_INGRESS_PAYLOAD_BYTES,
            WorkloadStreamKind::Egress => MAX_EGRESS_CONTROL_PAYLOAD_BYTES,
            WorkloadStreamKind::ProxyAnnouncement => MAX_PROXY_ANNOUNCEMENT_PAYLOAD_BYTES,
        }
    }

    pub const fn envelope_limit(self) -> usize {
        self.payload_limit() + WORKLOAD_ENVELOPE_OVERHEAD_BYTES
    }

    pub const fn response(self) -> Option<Self> {
        match self {
            Self::HandshakeRequest => Some(Self::HandshakeResponse),
            Self::RegistrationRequest => Some(Self::RegistrationResponse),
            Self::ProxyDiscoveryRequest => Some(Self::ProxyDiscoveryResponse),
            Self::IngressRequest => Some(Self::IngressResponse),
            Self::EgressRequest => Some(Self::EgressResponse),
            Self::ProxyAnnouncementRequest => Some(Self::ProxyAnnouncementResponse),
            _ => None,
        }
    }

    pub const fn is_request(self) -> bool {
        self.response().is_some()
    }
}

pub struct WorkloadEnvelopeParts<'a> {
    pub nonce: &'a str,
    pub now_millis: u64,
    pub sender_id: &'a str,
    pub recipient_id: &'a str,
    pub sender_signing_public: &'a [u8],
    pub sender_signing_private: &'a [u8],
    pub sender_kem_public: Option<&'a [u8]>,
}

#[derive(Debug)]
pub struct AcceptedWorkloadPayload<T> {
    pub payload: T,
    pub payload_type: WorkloadPayloadType,
    pub sender_signing_key: Vec<u8>,
    pub sender_kem_key: Option<Vec<u8>>,
    pub nonce: String,
    pub timestamp_millis: u64,
}

pub fn seal_workload_payload<T: WorkloadPayload>(
    parts: WorkloadEnvelopeParts<'_>,
    payload: &T,
) -> Result<Vec<u8>> {
    validate_endpoint_id(parts.sender_id, "sender")?;
    validate_endpoint_id(parts.recipient_id, "recipient")?;
    ensure!(
        !parts.nonce.is_empty() && parts.nonce.len() <= 128,
        "workload envelope nonce length is invalid"
    );
    validate_key(parts.sender_signing_public, "signing")?;
    if let Some(key) = parts.sender_kem_public {
        validate_key(key, "KEM")?;
    }
    payload.validate(parts.now_millis / 1000)?;
    let payload = postcard::to_allocvec(payload).context("serialize workload payload")?;
    ensure!(
        payload.len() <= T::TYPE.payload_limit(),
        "workload payload exceeds its type limit"
    );
    Envelope::seal(
        EnvelopeParts {
            payload: &payload,
            payload_type: T::TYPE.wire_name(),
            nonce: parts.nonce,
            ts_millis: parts.now_millis,
            sender_id: parts.sender_id,
            recipient_id: parts.recipient_id,
            sender_signing_pubkey: parts.sender_signing_public,
            sender_kem_pubkey: parts.sender_kem_public,
        },
        parts.sender_signing_private,
    )?
    .to_bytes_with_limit(T::TYPE.envelope_limit())
}

pub fn accept_workload_payload<T: WorkloadPayload>(
    bytes: &[u8],
    local_id: &str,
    authenticated_remote_id: &str,
    now_millis: u64,
    replay: &PeerReplayRegistry,
    observed_at: Instant,
) -> Result<AcceptedWorkloadPayload<T>> {
    let expected = T::TYPE;
    validate_endpoint_id(local_id, "local")?;
    validate_endpoint_id(authenticated_remote_id, "remote")?;
    let envelope = Envelope::decode_with_limit(bytes, expected.envelope_limit())?;
    ensure!(
        envelope.version == ENVELOPE_VERSION,
        "unsupported workload envelope version"
    );
    ensure!(
        envelope.alg == ENVELOPE_SIGNATURE_ALGORITHM,
        "unsupported workload envelope algorithm"
    );
    ensure!(
        WorkloadPayloadType::from_wire_name(&envelope.payload_type) == Some(expected),
        "unexpected workload payload type"
    );
    ensure!(
        envelope.sender_id == authenticated_remote_id,
        "workload envelope sender does not match transport"
    );
    ensure!(
        envelope.recipient_id == local_id,
        "workload envelope recipient does not match local endpoint"
    );
    ensure!(
        now_millis.abs_diff(envelope.ts_millis) <= MAX_ENVELOPE_DRIFT_MS,
        "workload envelope timestamp is outside drift window"
    );
    ensure!(
        !envelope.nonce.is_empty() && envelope.nonce.len() <= replay.limits().max_nonce_bytes,
        "workload envelope nonce length is invalid"
    );
    validate_canonical_key(&envelope.sender_signing_pubkey, "signing")?;
    if !envelope.sender_kem_pubkey.is_empty() {
        validate_canonical_key(&envelope.sender_kem_pubkey, "KEM")?;
    }
    validate_canonical_signature(&envelope.signature)?;
    envelope.verify_signature()?;
    replay.check_and_insert(authenticated_remote_id, &envelope.nonce, observed_at)?;
    ensure!(
        envelope.payload.len() <= expected.payload_limit(),
        "workload payload exceeds its type limit"
    );
    let payload: T = postcard::from_bytes(&envelope.payload).context("decode workload payload")?;
    payload.validate(now_millis / 1000)?;
    Ok(AcceptedWorkloadPayload {
        payload,
        payload_type: expected,
        sender_signing_key: envelope.sender_signing_key()?,
        sender_kem_key: envelope.sender_kem_key()?,
        nonce: envelope.nonce,
        timestamp_millis: envelope.ts_millis,
    })
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ProxyAnnouncementRequest {
    pub endpoint: EndpointRecord,
}

impl WorkloadPayload for ProxyAnnouncementRequest {
    const TYPE: WorkloadPayloadType = WorkloadPayloadType::ProxyAnnouncementRequest;

    fn validate(&self, now_secs: u64) -> Result<()> {
        self.endpoint.verify(now_secs)
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ProxyAnnouncementResponse {
    pub endpoint: EndpointRecord,
}

impl WorkloadPayload for ProxyAnnouncementResponse {
    const TYPE: WorkloadPayloadType = WorkloadPayloadType::ProxyAnnouncementResponse;

    fn validate(&self, now_secs: u64) -> Result<()> {
        self.endpoint.verify(now_secs)
    }
}

fn validate_endpoint_id(value: &str, name: &str) -> Result<()> {
    ensure!(
        value.len() == ENDPOINT_ID_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "workload envelope {name} endpoint id is invalid"
    );
    Ok(())
}

fn validate_key(key: &[u8], name: &str) -> Result<()> {
    ensure!(
        key.len() == crypto::ED25519_PUBLIC_KEY_SIZE,
        "workload envelope {name} key length is invalid"
    );
    Ok(())
}

fn validate_canonical_key(value: &str, name: &str) -> Result<()> {
    let decoded = crypto::b64_decode(value)?;
    validate_key(&decoded, name)?;
    ensure!(
        crypto::b64_encode(&decoded) == value,
        "workload envelope {name} key is not canonical Base64"
    );
    Ok(())
}

fn validate_canonical_signature(value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "workload envelope is unsigned");
    let decoded = crypto::b64_decode(value)?;
    ensure!(
        decoded.len() == crypto::ED25519_SIGNATURE_SIZE,
        "workload envelope signature length is invalid"
    );
    ensure!(
        crypto::b64_encode(&decoded) == value,
        "workload envelope signature is not canonical Base64"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize)]
    struct Payload {
        value: String,
    }

    impl WorkloadPayload for Payload {
        const TYPE: WorkloadPayloadType = WorkloadPayloadType::RegistrationRequest;

        fn validate(&self, _now_secs: u64) -> Result<()> {
            ensure!(!self.value.is_empty(), "test payload is empty");
            Ok(())
        }
    }

    #[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize)]
    struct ResponsePayload {
        value: String,
    }

    impl WorkloadPayload for ResponsePayload {
        const TYPE: WorkloadPayloadType = WorkloadPayloadType::HandshakeResponse;

        fn validate(&self, _now_secs: u64) -> Result<()> {
            Ok(())
        }
    }

    fn endpoint(byte: u8) -> String {
        format!("{byte:02x}").repeat(32)
    }

    fn assert_payload_type<T: WorkloadPayload>(expected: WorkloadPayloadType) {
        assert_eq!(T::TYPE, expected);
    }

    #[test]
    fn registry_is_closed_unique_and_paired() {
        let mut names = WorkloadPayloadType::ALL
            .map(WorkloadPayloadType::wire_name)
            .to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), WorkloadPayloadType::ALL.len());
        assert_eq!(
            WorkloadPayloadType::ALL
                .into_iter()
                .filter(|payload_type| payload_type.is_request())
                .count(),
            6
        );
        for payload_type in WorkloadPayloadType::ALL {
            assert_eq!(
                WorkloadPayloadType::from_wire_name(payload_type.wire_name()),
                Some(payload_type)
            );
            assert_eq!(
                payload_type.frame_kind().payload_limit(),
                payload_type.envelope_limit()
            );
        }
    }

    #[test]
    fn every_concrete_payload_has_one_compile_time_type() {
        assert_payload_type::<crate::machine::WorkloadHandshakeRequest>(
            WorkloadPayloadType::HandshakeRequest,
        );
        assert_payload_type::<crate::machine::WorkloadHandshakeResponse>(
            WorkloadPayloadType::HandshakeResponse,
        );
        assert_payload_type::<crate::SidecarRegistration>(WorkloadPayloadType::RegistrationRequest);
        assert_payload_type::<crate::SidecarRegistrationAck>(
            WorkloadPayloadType::RegistrationResponse,
        );
        assert_payload_type::<crate::ProxyDiscoveryRequest>(
            WorkloadPayloadType::ProxyDiscoveryRequest,
        );
        assert_payload_type::<crate::ProxyEndpointDiscoveryResponse>(
            WorkloadPayloadType::ProxyDiscoveryResponse,
        );
        assert_payload_type::<crate::IngressRequestMetadata>(WorkloadPayloadType::IngressRequest);
        assert_payload_type::<crate::IngressResponseMetadata>(WorkloadPayloadType::IngressResponse);
        assert_payload_type::<crate::egress::EgressTunnelRequest>(
            WorkloadPayloadType::EgressRequest,
        );
        assert_payload_type::<crate::egress::EgressTunnelResponse>(
            WorkloadPayloadType::EgressResponse,
        );
        assert_payload_type::<ProxyAnnouncementRequest>(
            WorkloadPayloadType::ProxyAnnouncementRequest,
        );
        assert_payload_type::<ProxyAnnouncementResponse>(
            WorkloadPayloadType::ProxyAnnouncementResponse,
        );
    }

    #[test]
    fn registry_exposes_the_approved_limits() {
        for payload_type in WorkloadPayloadType::ALL {
            assert_eq!(
                payload_type.envelope_limit(),
                payload_type.payload_limit() + WORKLOAD_ENVELOPE_OVERHEAD_BYTES
            );
        }
        assert_eq!(
            WorkloadPayloadType::HandshakeRequest.payload_limit(),
            64 * 1024
        );
        assert_eq!(
            WorkloadPayloadType::IngressResponse.payload_limit(),
            64 * 1024
        );
        assert_eq!(
            WorkloadPayloadType::EgressRequest.payload_limit(),
            32 * 1024
        );
        assert_eq!(
            WorkloadPayloadType::ProxyAnnouncementResponse.payload_limit(),
            4 * 1024
        );
    }

    #[test]
    fn typed_payload_seals_and_accepts_once() {
        let (public, private) = crypto::generate_signing_keypair();
        let sender = endpoint(1);
        let recipient = endpoint(2);
        let now = 1_700_000_000_000;
        let bytes = seal_workload_payload(
            WorkloadEnvelopeParts {
                nonce: "12345678-1234-1234-1234-123456789012",
                now_millis: now,
                sender_id: &sender,
                recipient_id: &recipient,
                sender_signing_public: &public,
                sender_signing_private: &private,
                sender_kem_public: None,
            },
            &Payload {
                value: "payload".into(),
            },
        )
        .unwrap();
        let replay = PeerReplayRegistry::new(crate::ReplayLimits::default()).unwrap();
        let accepted: AcceptedWorkloadPayload<Payload> =
            accept_workload_payload(&bytes, &recipient, &sender, now, &replay, Instant::now())
                .unwrap();
        assert_eq!(accepted.payload.value, "payload");
        assert!(
            accept_workload_payload::<Payload>(
                &bytes,
                &recipient,
                &sender,
                now,
                &replay,
                Instant::now(),
            )
            .is_err()
        );
    }

    #[test]
    fn response_is_refused_as_request() {
        let (public, private) = crypto::generate_signing_keypair();
        let sender = endpoint(3);
        let recipient = endpoint(4);
        let now = 1_700_000_000_000;
        let bytes = seal_workload_payload(
            WorkloadEnvelopeParts {
                nonce: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                now_millis: now,
                sender_id: &sender,
                recipient_id: &recipient,
                sender_signing_public: &public,
                sender_signing_private: &private,
                sender_kem_public: None,
            },
            &ResponsePayload {
                value: "response".into(),
            },
        )
        .unwrap();
        let replay = PeerReplayRegistry::new(crate::ReplayLimits::default()).unwrap();
        assert!(
            accept_workload_payload::<Payload>(
                &bytes,
                &recipient,
                &sender,
                now,
                &replay,
                Instant::now(),
            )
            .is_err()
        );
    }

    #[test]
    fn canonical_acceptance_refuses_mutated_security_fields() {
        let (public, private) = crypto::generate_signing_keypair();
        let sender = endpoint(5);
        let recipient = endpoint(6);
        let now = 1_700_000_000_000;
        let bytes = seal_workload_payload(
            WorkloadEnvelopeParts {
                nonce: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                now_millis: now,
                sender_id: &sender,
                recipient_id: &recipient,
                sender_signing_public: &public,
                sender_signing_private: &private,
                sender_kem_public: None,
            },
            &Payload {
                value: "request".into(),
            },
        )
        .unwrap();

        for mutate in [
            |envelope: &mut Envelope| envelope.version += 1,
            |envelope: &mut Envelope| envelope.alg = "other".into(),
            |envelope: &mut Envelope| envelope.sender_id = endpoint(7),
            |envelope: &mut Envelope| envelope.recipient_id = endpoint(8),
            |envelope: &mut Envelope| {
                envelope.ts_millis = 1_700_000_000_000 - MAX_ENVELOPE_DRIFT_MS - 1
            },
            |envelope: &mut Envelope| envelope.payload.push(1),
            |envelope: &mut Envelope| envelope.signature = "invalid".into(),
        ] {
            let mut envelope = Envelope::decode_with_limit(
                &bytes,
                WorkloadPayloadType::RegistrationRequest.envelope_limit(),
            )
            .unwrap();
            mutate(&mut envelope);
            let mutated = envelope
                .to_bytes_with_limit(WorkloadPayloadType::RegistrationRequest.envelope_limit())
                .unwrap();
            let replay = PeerReplayRegistry::new(crate::ReplayLimits::default()).unwrap();
            assert!(
                accept_workload_payload::<Payload>(
                    &mutated,
                    &recipient,
                    &sender,
                    now,
                    &replay,
                    Instant::now(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn payload_larger_than_its_operation_limit_is_refused_before_sealing() {
        #[derive(serde::Deserialize, serde::Serialize)]
        struct AnnouncementSizedPayload(Vec<u8>);

        impl WorkloadPayload for AnnouncementSizedPayload {
            const TYPE: WorkloadPayloadType = WorkloadPayloadType::ProxyAnnouncementRequest;

            fn validate(&self, _now_secs: u64) -> Result<()> {
                Ok(())
            }
        }

        let (public, private) = crypto::generate_signing_keypair();
        let error = seal_workload_payload(
            WorkloadEnvelopeParts {
                nonce: "cccccccc-cccc-cccc-cccc-cccccccccccc",
                now_millis: 1_700_000_000_000,
                sender_id: &endpoint(9),
                recipient_id: &endpoint(10),
                sender_signing_public: &public,
                sender_signing_private: &private,
                sender_kem_public: None,
            },
            &AnnouncementSizedPayload(vec![0; MAX_PROXY_ANNOUNCEMENT_PAYLOAD_BYTES + 1]),
        )
        .unwrap_err();
        assert!(error.to_string().contains("type limit"));
    }
}
