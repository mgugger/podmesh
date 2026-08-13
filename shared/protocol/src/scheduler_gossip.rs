//! Messages carried on the scheduler gossip topic.
//!
//! Gossip is the only channel every scheduler shares, so it carries the three
//! things that must reach the whole mesh: placement queries, membership
//! announcements, and requests to locate an agent's attachment.
//!
//! Locating over gossip rather than by probing peers directly is what lets the
//! mesh grow. A probe per peer costs one QUIC connection each, so a mesh of a
//! few thousand schedulers would spend thousands of connections answering one
//! client request. A gossip broadcast costs each scheduler one message, and only
//! the scheduler that actually holds the attachment answers.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::capacity::MAX_CAPACITY_MESSAGE_BYTES;
use crate::{CapacityQuery, EndpointRecord, IROH_ENDPOINT_ID_BYTES};

pub const SCHEDULER_GOSSIP_PROTOCOL_VERSION: u16 = 1;

/// Longest a location query stays answerable.
pub const MAX_LOCATION_QUERY_LIFETIME_SECS: u64 = 15;
/// Tolerated clock difference between schedulers for a location query.
pub const MAX_LOCATION_CLOCK_SKEW_SECS: u64 = 5;
/// Longest accepted query id. A uuid needs 36 bytes; the rest is slack.
pub const MAX_LOCATION_QUERY_ID_BYTES: usize = 128;

/// Anything a scheduler broadcasts to the mesh.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SchedulerGossipMessage {
    /// Solicit capacity offers for a placement.
    Capacity(Box<CapacityQuery>),
    /// "I am a scheduler, here is how to reach me."
    ///
    /// Membership would otherwise be limited to the peers an operator wrote
    /// down on every node, which is quadratic work and caps the mesh at the
    /// number of configured URLs.
    Announcement(Box<EndpointRecord>),
    /// Ask which scheduler holds an agent's attachment.
    Locate(Box<AgentLocationQuery>),
}

impl SchedulerGossipMessage {
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let bytes = postcard::to_allocvec(self).context("serialize scheduler gossip message")?;
        ensure!(
            bytes.len() <= MAX_CAPACITY_MESSAGE_BYTES,
            "scheduler gossip message exceeds {MAX_CAPACITY_MESSAGE_BYTES} bytes"
        );
        Ok(bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_CAPACITY_MESSAGE_BYTES,
            "scheduler gossip message size is invalid"
        );
        postcard::from_bytes(bytes).context("decode scheduler gossip message")
    }
}

/// Asks the mesh which scheduler currently holds an agent's attachment.
///
/// Signed so a non-member cannot inject one, and short-lived and nonced so a
/// captured query cannot be replayed to make schedulers dial an endpoint of the
/// attacker's choosing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLocationQuery {
    pub version: u16,
    pub query_id: String,
    #[serde(with = "serde_bytes")]
    pub agent_endpoint_id: Vec<u8>,
    /// Where the holder should answer. Signed, so the answer cannot be steered
    /// to a third party.
    pub reply_endpoint: EndpointRecord,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub signing_pubkey: String,
    pub signature: String,
}

impl AgentLocationQuery {
    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        postcard::to_allocvec(&Self {
            signature: String::new(),
            ..self.clone()
        })
        .context("serialize canonical agent location query")
    }

    pub fn sign(
        mut self,
        signing_public: &[u8],
        signing_private: &[u8],
        now_secs: u64,
    ) -> Result<Self> {
        self.signing_pubkey = crypto::b64_encode(signing_public);
        self.signature.clear();
        self.validate_unsigned(now_secs)?;
        self.signature = crypto::b64_encode(&crypto::sign_domain(
            signing_private,
            crypto::SignatureDomain::AgentLocationQuery,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> Result<()> {
        self.validate_unsigned(now_secs)?;
        let public = crypto::b64_decode(&self.signing_pubkey)?;
        let signature = crypto::b64_decode(&self.signature)?;
        crypto::verify_domain(
            &public,
            crypto::SignatureDomain::AgentLocationQuery,
            &self.canonical_bytes()?,
            &signature,
        )
    }

    fn validate_unsigned(&self, now_secs: u64) -> Result<()> {
        ensure!(
            self.version == SCHEDULER_GOSSIP_PROTOCOL_VERSION,
            "unsupported agent location query version"
        );
        ensure!(
            self.agent_endpoint_id.len() == IROH_ENDPOINT_ID_BYTES,
            "agent EndpointId must contain {IROH_ENDPOINT_ID_BYTES} bytes"
        );
        ensure!(
            !self.query_id.is_empty() && self.query_id.len() <= MAX_LOCATION_QUERY_ID_BYTES,
            "location query id must be 1 to {MAX_LOCATION_QUERY_ID_BYTES} bytes"
        );
        ensure!(
            self.issued_at_secs <= now_secs.saturating_add(MAX_LOCATION_CLOCK_SKEW_SECS),
            "agent location query issued too far in the future"
        );
        ensure!(
            self.expires_at_secs > self.issued_at_secs
                && self.expires_at_secs - self.issued_at_secs <= MAX_LOCATION_QUERY_LIFETIME_SECS,
            "agent location query lifetime is invalid"
        );
        ensure!(
            self.expires_at_secs
                .saturating_add(MAX_LOCATION_CLOCK_SKEW_SECS)
                >= now_secs,
            "agent location query expired"
        );
        self.reply_endpoint.verify(now_secs)?;
        // The reply endpoint must belong to the signer, so an answer cannot be
        // steered at a peer that did not ask.
        ensure!(
            self.reply_endpoint.signing_pubkey == self.signing_pubkey,
            "agent location reply endpoint does not belong to the query signer"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ENDPOINT_RECORD_VERSION;

    const NOW: u64 = 1_700_000_000;

    fn query(signing_public: &[u8], signing_private: &[u8]) -> AgentLocationQuery {
        let reply_endpoint = EndpointRecord {
            version: ENDPOINT_RECORD_VERSION,
            endpoint_id: vec![7; IROH_ENDPOINT_ID_BYTES],
            relay_url: None,
            direct_addresses: vec!["127.0.0.1:4000".into()],
            signing_pubkey: String::new(),
            issued_at_secs: NOW,
            expires_at_secs: NOW + 60,
            signature: String::new(),
        }
        .sign(signing_public, signing_private, NOW)
        .unwrap();
        AgentLocationQuery {
            version: SCHEDULER_GOSSIP_PROTOCOL_VERSION,
            query_id: "locate-1".into(),
            agent_endpoint_id: vec![3; IROH_ENDPOINT_ID_BYTES],
            reply_endpoint,
            issued_at_secs: NOW,
            expires_at_secs: NOW + 5,
            signing_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(signing_public, signing_private, NOW)
        .unwrap()
    }

    #[test]
    fn a_signed_query_round_trips_through_the_gossip_envelope() {
        let (public, private) = crypto::generate_signing_keypair();
        let message = SchedulerGossipMessage::Locate(Box::new(query(&public, &private)));
        let decoded = SchedulerGossipMessage::from_bytes(&message.to_bytes().unwrap()).unwrap();
        assert_eq!(decoded, message);
        match decoded {
            SchedulerGossipMessage::Locate(query) => query.verify(NOW).unwrap(),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn a_tampered_query_is_refused() {
        let (public, private) = crypto::generate_signing_keypair();
        let mut tampered = query(&public, &private);
        tampered.agent_endpoint_id = vec![9; IROH_ENDPOINT_ID_BYTES];
        assert!(tampered.verify(NOW).is_err());
    }

    /// The answer goes to whoever the query names, so a query must not be able
    /// to name somebody else and turn the mesh into a reflector.
    #[test]
    fn a_reply_endpoint_belonging_to_another_key_is_refused() {
        let (public, private) = crypto::generate_signing_keypair();
        let (other_public, other_private) = crypto::generate_signing_keypair();
        let mut query = query(&public, &private);
        query.reply_endpoint = EndpointRecord {
            version: ENDPOINT_RECORD_VERSION,
            endpoint_id: vec![7; IROH_ENDPOINT_ID_BYTES],
            relay_url: None,
            direct_addresses: vec!["127.0.0.1:4000".into()],
            signing_pubkey: String::new(),
            issued_at_secs: NOW,
            expires_at_secs: NOW + 60,
            signature: String::new(),
        }
        .sign(&other_public, &other_private, NOW)
        .unwrap();
        let query = query.sign(&public, &private, NOW);
        assert!(
            query.is_err(),
            "signing must refuse a foreign reply endpoint"
        );
    }

    #[test]
    fn an_expired_query_is_refused() {
        let (public, private) = crypto::generate_signing_keypair();
        let query = query(&public, &private);
        assert!(
            query
                .verify(NOW + MAX_LOCATION_QUERY_LIFETIME_SECS + MAX_LOCATION_CLOCK_SKEW_SECS + 10)
                .is_err()
        );
    }
}
