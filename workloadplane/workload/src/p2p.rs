use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use libp2p::{
    Multiaddr, Swarm, autonat, gossipsub, identify, kad, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use p2p::{CoreBehaviourAccess, NodeConfig};
use protocol::libp2p_constants::BEEMESH_CLUSTER;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::Config;

#[derive(NetworkBehaviour)]
pub struct WorkloadBehaviour {
    pub gossipsub: gossipsub::Behaviour,
    pub kademlia: kad::Behaviour<kad::store::MemoryStore>,
    pub relay: relay::Behaviour,
    pub autonat: autonat::Behaviour,
    pub identify: identify::Behaviour,
}

impl CoreBehaviourAccess for WorkloadBehaviour {
    fn gossipsub_mut(&mut self) -> &mut gossipsub::Behaviour {
        &mut self.gossipsub
    }

    fn kademlia_mut(&mut self) -> &mut kad::Behaviour<kad::store::MemoryStore> {
        &mut self.kademlia
    }
}

pub struct P2pNodeHandle {
    task: JoinHandle<()>,
    peer_rx: watch::Receiver<Vec<String>>,
    peer_id: String,
}

impl P2pNodeHandle {
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    pub fn peer_rx(&self) -> watch::Receiver<Vec<String>> {
        self.peer_rx.clone()
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

pub fn spawn(cfg: &Config) -> Result<P2pNodeHandle> {
    let node_cfg = NodeConfig::new(
        cfg.libp2p_quic_port,
        cfg.libp2p_host.clone(),
        BEEMESH_CLUSTER,
    );
    let (mut swarm, _topic, peer_rx, peer_tx) = p2p::setup_swarm(node_cfg, |key| {
        let message_id_fn = |message: &gossipsub::Message| {
            let mut s = DefaultHasher::new();
            message.data.hash(&mut s);
            gossipsub::MessageId::from(s.finish().to_string())
        };
        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(10))
            .validation_mode(gossipsub::ValidationMode::Strict)
            .mesh_n_low(1)
            .mesh_n(3)
            .mesh_n_high(6)
            .mesh_outbound_min(1)
            .message_id_fn(message_id_fn)
            .allow_self_origin(true)
            .build()
            .expect("valid workload gossipsub config");
        let gossipsub = gossipsub::Behaviour::new(
            gossipsub::MessageAuthenticity::Signed(key.clone()),
            gossipsub_config,
        )
        .expect("create workload gossipsub behaviour");

        let store = kad::store::MemoryStore::new(key.public().to_peer_id());
        let mut kademlia_config = kad::Config::default();
        kademlia_config.set_parallelism(std::num::NonZeroUsize::new(3).unwrap());
        kademlia_config.set_query_timeout(Duration::from_secs(15));
        let kademlia =
            kad::Behaviour::with_config(key.public().to_peer_id(), store, kademlia_config);

        let relay = relay::Behaviour::new(key.public().to_peer_id(), Default::default());
        let autonat = autonat::Behaviour::new(key.public().to_peer_id(), Default::default());
        let identify =
            identify::Behaviour::new(identify::Config::new("/beemesh/0.1.0".into(), key.public()));

        WorkloadBehaviour {
            gossipsub,
            kademlia,
            relay,
            autonat,
            identify,
        }
    })?;

    for addr in &cfg.bootstrap_peer_strings {
        match addr.parse::<Multiaddr>() {
            Ok(ma) => {
                if let Err(err) = swarm.dial(ma) {
                    warn!("failed to dial bootstrap {}: {}", addr, err);
                }
            }
            Err(err) => warn!("invalid bootstrap multiaddr {}: {}", addr, err),
        }
    }

    let mut peer_tx = peer_tx;
    let local_peer_id = swarm.local_peer_id().to_string();
    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                event = swarm.select_next_some() => match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        info!("workload libp2p listening on {}", address);
                    }
                    SwarmEvent::Behaviour(WorkloadBehaviourEvent::Gossipsub(gossipsub::Event::Subscribed { peer_id, .. })) => {
                        debug!("peer {} subscribed", peer_id);
                        publish_peer_snapshot(&swarm, &mut peer_tx);
                    }
                    SwarmEvent::Behaviour(WorkloadBehaviourEvent::Gossipsub(gossipsub::Event::Unsubscribed { peer_id, .. })) => {
                        debug!("peer {} unsubscribed", peer_id);
                        publish_peer_snapshot(&swarm, &mut peer_tx);
                    }
                    SwarmEvent::Behaviour(WorkloadBehaviourEvent::Kademlia(event)) => {
                        trace_kad(event);
                    }
                    SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                        info!("connection established with {}", peer_id);
                        swarm.behaviour_mut().kademlia.add_address(&peer_id, endpoint.get_remote_address().clone());
                    }
                    SwarmEvent::ConnectionClosed { peer_id, .. } => {
                        info!("connection closed with {}", peer_id);
                        swarm.behaviour_mut().kademlia.remove_peer(&peer_id);
                    }
                    _ => {}
                },
                _ = interval.tick() => publish_peer_snapshot(&swarm, &mut peer_tx),
            }
        }
    });

    Ok(P2pNodeHandle {
        task,
        peer_rx,
        peer_id: local_peer_id,
    })
}

fn publish_peer_snapshot(
    swarm: &Swarm<WorkloadBehaviour>,
    peer_tx: &mut watch::Sender<Vec<String>>,
) {
    let peers: Vec<String> = swarm
        .behaviour()
        .gossipsub
        .all_peers()
        .map(|(peer, _)| peer.to_string())
        .collect();
    let _ = peer_tx.send(peers);
}

fn trace_kad(event: kad::Event) {
    match event {
        kad::Event::OutboundQueryProgressed { id, result, .. } => {
            debug!("kad query {:?} progressed: {:?}", id, result);
        }
        kad::Event::RoutingUpdated { peer, .. } => {
            debug!("kad routing updated for {}", peer);
        }
        _ => {}
    }
}
