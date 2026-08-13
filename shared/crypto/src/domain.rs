//! Signature domain separation.
//!
//! Every Podmesh message type is signed with the same Ed25519 identity key, and
//! every canonical form is a bare `postcard` encoding. Postcard emits no type
//! discriminator, so without an explicit domain tag a signature produced for one
//! message type could be replayed as a signature over a different type whose
//! encoding happens to collide. Each signed buffer is therefore prefixed with a
//! length-delimited domain label before it reaches Ed25519.

/// The label bound into a signature. Adding a variant is the only supported way
/// to introduce a new signed message type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureDomain {
    AdmissionRequest,
    Reservation,
    DeploymentGrant,
    DeploymentReceipt,
    WorkloadCommand,
    WorkloadCommandResponse,
    WorkloadListRequest,
    WorkloadListResponse,
    AgentLocationQuery,
    CapacityQuery,
    CapacityOffer,
    EndpointRecord,
    MachineRelayGrant,
    AgentAttachmentHello,
    WorkloadHandshake,
    SidecarRegistration,
}

impl SignatureDomain {
    /// Stable wire label. These strings are part of the protocol: changing one
    /// invalidates every signature previously produced under it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::AdmissionRequest => "podmesh/sig/v1/admission-request",
            Self::Reservation => "podmesh/sig/v1/reservation",
            Self::DeploymentGrant => "podmesh/sig/v1/deployment-grant",
            Self::DeploymentReceipt => "podmesh/sig/v1/deployment-receipt",
            Self::WorkloadCommand => "podmesh/sig/v1/workload-command",
            Self::WorkloadCommandResponse => "podmesh/sig/v1/workload-command-response",
            Self::WorkloadListRequest => "podmesh/sig/v1/workload-list-request",
            Self::WorkloadListResponse => "podmesh/sig/v1/workload-list-response",
            Self::AgentLocationQuery => "podmesh/sig/v1/agent-location-query",
            Self::CapacityQuery => "podmesh/sig/v1/capacity-query",
            Self::CapacityOffer => "podmesh/sig/v1/capacity-offer",
            Self::EndpointRecord => "podmesh/sig/v1/endpoint-record",
            Self::MachineRelayGrant => "podmesh/sig/v1/machine-relay-grant",
            Self::AgentAttachmentHello => "podmesh/sig/v1/agent-attachment-hello",
            Self::WorkloadHandshake => "podmesh/sig/v1/workload-handshake",
            Self::SidecarRegistration => "podmesh/sig/v1/sidecar-registration",
        }
    }

    /// Build the exact byte string that Ed25519 signs: the label length as a
    /// little-endian `u16`, the label, then the message. Length-prefixing the
    /// label makes the concatenation unambiguous even if a future label were to
    /// become a prefix of another.
    pub fn bind(self, message: &[u8]) -> Vec<u8> {
        let label = self.label().as_bytes();
        let mut bound = Vec::with_capacity(2 + label.len() + message.len());
        bound.extend_from_slice(&(label.len() as u16).to_le_bytes());
        bound.extend_from_slice(label);
        bound.extend_from_slice(message);
        bound
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[SignatureDomain] = &[
        SignatureDomain::AdmissionRequest,
        SignatureDomain::Reservation,
        SignatureDomain::DeploymentGrant,
        SignatureDomain::DeploymentReceipt,
        SignatureDomain::WorkloadCommand,
        SignatureDomain::WorkloadCommandResponse,
        SignatureDomain::WorkloadListRequest,
        SignatureDomain::WorkloadListResponse,
        SignatureDomain::AgentLocationQuery,
        SignatureDomain::CapacityQuery,
        SignatureDomain::CapacityOffer,
        SignatureDomain::EndpointRecord,
        SignatureDomain::MachineRelayGrant,
        SignatureDomain::AgentAttachmentHello,
        SignatureDomain::WorkloadHandshake,
        SignatureDomain::SidecarRegistration,
    ];

    #[test]
    fn labels_are_unique() {
        let mut labels: Vec<&str> = ALL.iter().map(|domain| domain.label()).collect();
        labels.sort_unstable();
        let total = labels.len();
        labels.dedup();
        assert_eq!(
            labels.len(),
            total,
            "signature domain labels must be unique"
        );
    }

    #[test]
    fn distinct_domains_bind_distinct_bytes() {
        let message = b"identical message body";
        for (index, left) in ALL.iter().enumerate() {
            for right in ALL.iter().skip(index + 1) {
                assert_ne!(left.bind(message), right.bind(message));
            }
        }
    }

    #[test]
    fn binding_is_prefix_unambiguous() {
        // A message that starts with another domain's label must not collide
        // with that domain binding an empty message.
        let domain = SignatureDomain::CapacityOffer;
        let smuggled = SignatureDomain::CapacityQuery.bind(b"");
        assert_ne!(
            domain.bind(&smuggled),
            SignatureDomain::CapacityQuery.bind(b"")
        );
    }
}
