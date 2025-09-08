use futures::stream::StreamExt;
use libp2p::{
    gossipsub, mdns, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId, Swarm,
};
use std::{
    collections::hash_map::DefaultHasher,
    error::Error,
    hash::{Hash, Hasher},
    time::Duration,
};
use tokio::sync::watch;

#[derive(NetworkBehaviour)]
pub struct MyBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
}

pub fn setup_libp2p_node() -> Result<
    (
        Swarm<MyBehaviour>,
        gossipsub::IdentTopic,
        watch::Receiver<Vec<String>>,
        watch::Sender<Vec<String>>,
    ),
    Box<dyn Error>,
> {
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_behaviour(|key| {
            println!("Local PeerId: {}", key.public().to_peer_id());
            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(s.finish().to_string())
            };
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10))
                .validation_mode(gossipsub::ValidationMode::None)
                .mesh_n_low(0) // don’t try to maintain minimum peers
                .mesh_n(0) // target mesh size = 0
                .mesh_n_high(0)
                .message_id_fn(message_id_fn)
                .build()
                .map_err(|e| std::io::Error::other(e))?;
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )?;
            let mdns =
                mdns::tokio::Behaviour::new(mdns::Config::default(), key.public().to_peer_id())?;
            Ok(MyBehaviour { gossipsub, mdns })
        })?
        .build();

    let topic = gossipsub::IdentTopic::new("podmesh-cluster");
    println!("Subscribing to topic: {}", topic.hash());
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    let (peer_tx, peer_rx) = watch::channel(Vec::new());
    println!("Libp2p gossip node started. Listening for messages...");
    Ok((swarm, topic, peer_rx, peer_tx))
}

pub async fn start_libp2p_node(
    mut swarm: Swarm<MyBehaviour>,
    topic: gossipsub::IdentTopic,
    peer_tx: watch::Sender<Vec<String>>,
) -> Result<(), Box<dyn Error>> {
    use std::collections::HashMap;
    use tokio::time::Instant;

    struct HandshakeState {
        attempts: u8,
        last_attempt: Instant,
        confirmed: bool,
    }

    let mut handshake_states: HashMap<PeerId, HandshakeState> = HashMap::new();
    let mut handshake_interval = tokio::time::interval(Duration::from_secs(1));
    //let mut mesh_alive_interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                        for (peer_id, _multiaddr) in list {
                            swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                            handshake_states.entry(peer_id.clone()).or_insert(HandshakeState {
                                attempts: 0,
                                last_attempt: Instant::now() - Duration::from_secs(3),
                                confirmed: false,
                            });
                        }
                        // Update peer list in channel
                        let topic = gossipsub::IdentTopic::new("podmesh-cluster");
                        let peers: Vec<String> = swarm.behaviour().gossipsub.mesh_peers(&topic.hash()).map(|p| p.to_string()).collect();
                        let _ = peer_tx.send(peers);
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                        for (peer_id, _multiaddr) in list {
                            swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                            handshake_states.remove(&peer_id);
                        }
                        // Update peer list in channel
                        let topic = gossipsub::IdentTopic::new("podmesh-cluster");
                        let peers: Vec<String> = swarm.behaviour().gossipsub.mesh_peers(&topic.hash()).map(|p| p.to_string()).collect();
                        let _ = peer_tx.send(peers);
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                        propagation_source: peer_id,
                        message_id: id,
                        message,
                    })) => {
                        let msg_content = String::from_utf8_lossy(&message.data);
                        if msg_content.starts_with("podmesh-handshake") {
                            println!("Confirmed peer: {peer_id}");
                            let state = handshake_states.entry(peer_id.clone()).or_insert(HandshakeState {
                                attempts: 0,
                                last_attempt: Instant::now() - Duration::from_secs(3),
                                confirmed: false,
                            });
                            if !state.confirmed {
                                state.confirmed = true;
                                // Reply with handshake if not already confirmed
                                let reply_msg = format!("podmesh-handshake-{}-reply", peer_id);
                                let _ = swarm.behaviour_mut().gossipsub.publish(topic.clone(), reply_msg.as_bytes());
                            }
                        } else {
                            println!(
                                "Got message: '{}' with id: {id} from peer: {peer_id}",
                                msg_content
                            );
                        }
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Subscribed { peer_id, topic })) => {
                        println!("Peer {peer_id} subscribed to topic: {topic}");
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Unsubscribed { peer_id, topic })) => {
                        println!("Peer {peer_id} unsubscribed from topic: {topic}");
                    }
                    SwarmEvent::NewListenAddr { address, .. } => {
                        println!("Local node is listening on {address}");
                    }
                    _ => {}
                }
            }
            _ = handshake_interval.tick() => {
                let mut to_remove = Vec::new();
                for (peer_id, state) in handshake_states.iter_mut() {
                    if state.confirmed {
                        continue;
                    }
                    if state.attempts >= 3 {
                        println!("Removing non-responsive peer: {peer_id}");
                        swarm
                            .behaviour_mut()
                            .gossipsub
                            .remove_explicit_peer(peer_id);
                        to_remove.push(peer_id.clone());
                        continue;
                    }
                    if state.last_attempt.elapsed() >= Duration::from_secs(2) {
                        let handshake_msg =
                            format!("podmesh-handshake-{}-{}", peer_id, state.attempts + 1);
                        match swarm
                            .behaviour_mut()
                            .gossipsub
                            .publish(topic.clone(), handshake_msg.as_bytes())
                        {
                            Ok(_) => {
                                state.attempts += 1;
                                state.last_attempt = Instant::now();
                            }
                            Err(_e) => {
                                state.attempts += 1;
                                state.last_attempt = Instant::now();
                            }
                        }
                    }
                }
                for peer_id in to_remove {
                    handshake_states.remove(&peer_id);
                }
                // Update peer list in channel after handshake changes
                let all_peers: Vec<String> = swarm.behaviour().gossipsub.all_peers().map(|(p, _topics)| p.to_string()).collect();
                let _ = peer_tx.send(all_peers);
            }
            /*_ = mesh_alive_interval.tick() => {
                // Periodically publish a 'mesh-alive' message to the topic
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
                let mesh_alive_msg = format!("mesh-alive-{}-{}", swarm.local_peer_id(), now);
                let res = swarm.behaviour_mut().gossipsub.publish(topic.clone(), mesh_alive_msg.as_bytes());
            }*/
        }
    }
}
