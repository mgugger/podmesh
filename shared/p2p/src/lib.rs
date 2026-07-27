pub mod envelope;
pub mod handshake;
pub mod http_proxy;
pub mod identity;
pub mod message_verifier;
pub mod multiaddr;
pub mod request_response;
pub mod security;
pub mod util;

pub use libp2p_stream;
pub use multiaddr::build_quic_multiaddr;
pub use util::{timestamp_millis, timestamp_secs};

use anyhow::Result;
use libp2p::{Multiaddr, Swarm, gossipsub, swarm::NetworkBehaviour};
use log::debug;
use protocol::libp2p_constants::{
    GOSSIPSUB_HEARTBEAT_INTERVAL_SECS, GOSSIPSUB_MESH_N, GOSSIPSUB_MESH_N_HIGH,
    GOSSIPSUB_MESH_N_LOW, GOSSIPSUB_MESH_OUTBOUND_MIN,
};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;
use tokio::sync::watch;

/// Behaviour accessor for shared libp2p helpers.
pub trait CoreBehaviourAccess {
    fn gossipsub_mut(&mut self) -> &mut gossipsub::Behaviour;
}

/// Basic configuration for creating a libp2p node.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub quic_port: u16,
    pub host: String,
    pub topic: String,
}

impl NodeConfig {
    pub fn new(quic_port: u16, host: impl Into<String>, topic: impl Into<String>) -> Self {
        Self {
            quic_port,
            host: host.into(),
            topic: topic.into(),
        }
    }
}

pub type SwarmSetup<B> = (
    Swarm<B>,
    gossipsub::IdentTopic,
    watch::Receiver<Vec<String>>,
    watch::Sender<Vec<String>>,
);

/// Construct a libp2p swarm with caller-provided behaviour factory.
pub fn setup_swarm<B, F>(
    config: NodeConfig,
    identity: libp2p::identity::Keypair,
    behaviour_builder: F,
) -> Result<SwarmSetup<B>>
where
    B: NetworkBehaviour + CoreBehaviourAccess,
    F: FnOnce(&libp2p::identity::Keypair) -> B,
{
    let NodeConfig {
        quic_port,
        host,
        topic,
    } = config;

    let topic = gossipsub::IdentTopic::new(topic);

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(identity)
        .with_tokio()
        .with_quic()
        .with_dns()?
        .with_behaviour(behaviour_builder)?
        .build();

    debug!("Subscribing to topic: {}", topic.hash());
    swarm.behaviour_mut().gossipsub_mut().subscribe(&topic)?;
    let local_peer = *swarm.local_peer_id();
    swarm
        .behaviour_mut()
        .gossipsub_mut()
        .add_explicit_peer(&local_peer);

    let listen_addr = build_quic_multiaddr(&host, quic_port).ok_or_else(|| {
        anyhow::anyhow!("failed to build listen address for {}:{}", host, quic_port)
    })?;

    swarm.listen_on(listen_addr)?;

    let (peer_tx, peer_rx) = watch::channel(Vec::new());
    Ok((swarm, topic, peer_rx, peer_tx))
}

/// Create a default gossipsub configuration with podmesh-standard settings.
///
/// Uses constants from `protocol::libp2p_constants` for consistency across crates.
pub fn default_gossipsub_config() -> gossipsub::Config {
    let message_id_fn = |message: &gossipsub::Message| {
        let mut s = DefaultHasher::new();
        message.data.hash(&mut s);
        gossipsub::MessageId::from(s.finish().to_string())
    };

    gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(GOSSIPSUB_HEARTBEAT_INTERVAL_SECS))
        .validation_mode(gossipsub::ValidationMode::Strict)
        .mesh_n_low(GOSSIPSUB_MESH_N_LOW)
        .mesh_n(GOSSIPSUB_MESH_N)
        .mesh_n_high(GOSSIPSUB_MESH_N_HIGH)
        .mesh_outbound_min(GOSSIPSUB_MESH_OUTBOUND_MIN)
        .message_id_fn(message_id_fn)
        .allow_self_origin(true)
        .build()
        .expect("valid gossipsub config")
}

/// Create a gossipsub behaviour with default configuration and signed authentication.
pub fn create_gossipsub_behaviour(
    key: &libp2p::identity::Keypair,
) -> std::result::Result<gossipsub::Behaviour, &'static str> {
    gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(key.clone()),
        default_gossipsub_config(),
    )
}

/// Dial a multiaddr, logging errors on failure.
pub fn dial_multiaddr<B: NetworkBehaviour>(
    swarm: &mut Swarm<B>,
    addr: &Multiaddr,
) -> std::result::Result<(), libp2p::swarm::DialError> {
    swarm.dial(addr.clone())
}

/// Parse a multiaddr string and dial it.
pub fn dial_multiaddr_str<B: NetworkBehaviour>(
    swarm: &mut Swarm<B>,
    addr: &str,
) -> std::result::Result<(), String> {
    let ma: Multiaddr = addr
        .parse()
        .map_err(|e| format!("invalid multiaddr: {}", e))?;
    swarm.dial(ma).map_err(|e| format!("dial failed: {}", e))
}
