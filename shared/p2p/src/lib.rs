pub mod envelope;
pub mod handshake;
pub mod http_proxy;
pub mod message_verifier;
pub mod multiaddr;
pub mod request_response;
pub mod security;
pub mod util;

pub use multiaddr::{build_quic_multiaddr, parse_bootstrap_peer};
pub use util::{split_csv, timestamp_millis, timestamp_secs};

use anyhow::Result;
use libp2p::{Swarm, gossipsub, kad, swarm::NetworkBehaviour};
use log::debug;
use tokio::sync::watch;

/// Behaviour accessor for shared libp2p helpers.
pub trait CoreBehaviourAccess {
    fn gossipsub_mut(&mut self) -> &mut gossipsub::Behaviour;
    fn kademlia_mut(&mut self) -> &mut kad::Behaviour<kad::store::MemoryStore>;
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

/// Construct a libp2p swarm with caller-provided behaviour factory.
pub fn setup_swarm<B, F>(
    config: NodeConfig,
    behaviour_builder: F,
) -> Result<(
    Swarm<B>,
    gossipsub::IdentTopic,
    watch::Receiver<Vec<String>>,
    watch::Sender<Vec<String>>,
)>
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

    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_quic()
        .with_dns()?
        .with_behaviour(behaviour_builder)?
        .build();

    debug!("Subscribing to topic: {}", topic.hash());
    swarm.behaviour_mut().gossipsub_mut().subscribe(&topic)?;
    let local_peer = swarm.local_peer_id().clone();
    swarm
        .behaviour_mut()
        .gossipsub_mut()
        .add_explicit_peer(&local_peer);

    let listen_addr = build_quic_multiaddr(&host, quic_port)
        .ok_or_else(|| anyhow::anyhow!("failed to build listen address for {}:{}", host, quic_port))?;

    swarm.listen_on(listen_addr)?;

    let (peer_tx, peer_rx) = watch::channel(Vec::new());
    Ok((swarm, topic, peer_rx, peer_tx))
}
