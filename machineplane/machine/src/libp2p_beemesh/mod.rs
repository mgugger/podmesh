use futures::stream::StreamExt;
use libp2p::{
    gossipsub, mdns, noise, request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId, Swarm,
};
use std::collections::HashMap as StdHashMap;
use std::{
    collections::hash_map::DefaultHasher,
    error::Error,
    hash::{Hash, Hasher},
    time::Duration,
};
use tokio::sync::{mpsc, watch};

use protocol::libp2p_constants::BEEMESH_CLUSTER;

// Define a simple codec for apply request/response
#[derive(Debug, Clone, Default)]
pub struct ApplyCodec;

// Define a simple codec for handshake request/response
#[derive(Debug, Clone, Default)]
pub struct HandshakeCodec;

#[async_trait::async_trait]
impl request_response::Codec for HandshakeCodec {
    type Protocol = &'static str;
    type Request = Vec<u8>;
    type Response = Vec<u8>;

    async fn read_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Request>
    where
        T: futures::io::AsyncRead + Unpin + Send,
    {
        use futures::io::AsyncReadExt;
        let mut len_buf = [0u8; 4];
        io.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        io.read_exact(&mut buf).await?;
        Ok(buf)
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Response>
    where
        T: futures::io::AsyncRead + Unpin + Send,
    {
        use futures::io::AsyncReadExt;
        let mut len_buf = [0u8; 4];
        io.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        io.read_exact(&mut buf).await?;
        Ok(buf)
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> std::io::Result<()>
    where
        T: futures::io::AsyncWrite + Unpin + Send,
    {
        use futures::io::AsyncWriteExt;
        let len = req.len() as u32;
        io.write_all(&len.to_be_bytes()).await?;
        io.write_all(&req).await?;
        Ok(())
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> std::io::Result<()>
    where
        T: futures::io::AsyncWrite + Unpin + Send,
    {
        use futures::io::AsyncWriteExt;
        let len = res.len() as u32;
        io.write_all(&len.to_be_bytes()).await?;
        io.write_all(&res).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl request_response::Codec for ApplyCodec {
    type Protocol = &'static str;
    type Request = Vec<u8>;
    type Response = Vec<u8>;

    async fn read_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Request>
    where
        T: futures::io::AsyncRead + Unpin + Send,
    {
        use futures::io::AsyncReadExt;
        let mut len_buf = [0u8; 4];
        io.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        io.read_exact(&mut buf).await?;
        Ok(buf)
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Response>
    where
        T: futures::io::AsyncRead + Unpin + Send,
    {
        use futures::io::AsyncReadExt;
        let mut len_buf = [0u8; 4];
        io.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        io.read_exact(&mut buf).await?;
        Ok(buf)
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> std::io::Result<()>
    where
        T: futures::io::AsyncWrite + Unpin + Send,
    {
        use futures::io::AsyncWriteExt;
        let len = req.len() as u32;
        io.write_all(&len.to_be_bytes()).await?;
        io.write_all(&req).await?;
        Ok(())
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> std::io::Result<()>
    where
        T: futures::io::AsyncWrite + Unpin + Send,
    {
        use futures::io::AsyncWriteExt;
        let len = res.len() as u32;
        io.write_all(&len.to_be_bytes()).await?;
        io.write_all(&res).await?;
        Ok(())
    }
}

// Varint helpers (unsigned LEB128-like) for encoding arbitrary-length integers.
// No envelope varint helpers needed — we now embed request_id inside FlatBuffers and wrap

#[derive(NetworkBehaviour)]
pub struct MyBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
    apply_rr: request_response::Behaviour<ApplyCodec>,
    handshake_rr: request_response::Behaviour<HandshakeCodec>,
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
        .with_tcp(tcp::Config::default(), noise::Config::new, || {
            yamux::Config::default()
        })?
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
                .allow_self_origin(true)
                .build()
                .map_err(|e| std::io::Error::other(e))?;
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )?;
            let mdns =
                mdns::tokio::Behaviour::new(mdns::Config::default(), key.public().to_peer_id())?;

            // Create the request-response behavior for apply protocol
            let apply_rr = request_response::Behaviour::new(
                std::iter::once((
                    "/beemesh/apply/1.0.0",
                    request_response::ProtocolSupport::Full,
                )),
                request_response::Config::default(),
            );

            // Create the request-response behavior for handshake protocol
            let handshake_rr = request_response::Behaviour::new(
                std::iter::once((
                    "/beemesh/handshake/1.0.0",
                    request_response::ProtocolSupport::Full,
                )),
                request_response::Config::default(),
            );

            Ok(MyBehaviour {
                gossipsub,
                mdns,
                apply_rr,
                handshake_rr,
            })
        })?
        .build();

    let topic = gossipsub::IdentTopic::new(BEEMESH_CLUSTER);
    println!("Subscribing to topic: {}", topic.hash());
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;
    // Ensure local host is an explicit mesh peer for the topic so publish() finds at least one subscriber
    let local_peer = swarm.local_peer_id().clone();
    swarm
        .behaviour_mut()
        .gossipsub
        .add_explicit_peer(&local_peer);

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
    mut control_rx: mpsc::UnboundedReceiver<Libp2pControl>,
) -> Result<(), Box<dyn Error>> {
    use std::collections::HashMap;
    use tokio::time::Instant;

    struct HandshakeState {
        attempts: u8,
        last_attempt: Instant,
        confirmed: bool,
    }

    // pending queries: map request_id -> vec of reply_senders
    let mut pending_queries: StdHashMap<String, Vec<mpsc::UnboundedSender<String>>> =
        StdHashMap::new();

    let mut handshake_states: HashMap<PeerId, HandshakeState> = HashMap::new();
    let mut handshake_interval = tokio::time::interval(Duration::from_secs(1));
    //let mut mesh_alive_interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            // control messages from other parts of the host (REST handlers)
            maybe_msg = control_rx.recv() => {
                if let Some(msg) = maybe_msg {
                    match msg {
                        Libp2pControl::QueryCapacityWithPayload { request_id, reply_tx, payload } => {
                            // register the reply channel so incoming reply messages can be forwarded
                            println!("libp2p: control QueryCapacityWithPayload received request_id={} payload_len={}", request_id, payload.len());
                            pending_queries.entry(request_id.clone()).or_insert_with(Vec::new).push(reply_tx);

                            // Parse the provided payload as a CapacityRequest FlatBuffer and rebuild it into a TopicMessage wrapper
                            match protocol::machine::root_as_capacity_request(&payload) {
                                Ok(cap_req) => {
                                    // Rebuild a CapacityRequest flatbuffer embedding the request_id, and publish it directly (no wrapper)
                                    let mut fbb = flatbuffers::FlatBufferBuilder::new();
                                    let req_id_off = fbb.create_string(&request_id);
                                    let cpu = cap_req.cpu_milli();
                                    let mem = cap_req.memory_bytes();
                                    let stor = cap_req.storage_bytes();
                                    let reps = cap_req.replicas();
                                    let cap_args = protocol::machine::CapacityRequestArgs {
                                        request_id: Some(req_id_off),
                                        cpu_milli: cpu,
                                        memory_bytes: mem,
                                        storage_bytes: stor,
                                        replicas: reps,
                                    };
                                    let cap_off = protocol::machine::CapacityRequest::create(&mut fbb, &cap_args);
                                    protocol::machine::finish_capacity_request_buffer(&mut fbb, cap_off);
                                    let finished = fbb.finished_data().to_vec();
                                    let res = swarm.behaviour_mut().gossipsub.publish(topic.clone(), finished.as_slice());
                                    println!("libp2p: published capreq request_id={} publish_res={:?}", request_id, res);

                                    // Also notify local pending senders directly so the originator is always considered
                                    // a potential responder. This ensures single-node operation and makes the
                                    // origining host countable when collecting responders.
                                    if let Some(senders) = pending_queries.get_mut(&request_id) {
                                        for tx in senders.iter() {
                                            let _ = tx.send(swarm.local_peer_id().to_string());
                                        }
                                    }
                                }
                                Err(e) => {
                                    println!("libp2p: failed to parse provided capacity payload: {:?}", e);
                                }
                            }
                        }
                        Libp2pControl::SendApplyRequest { peer_id, manifest, reply_tx } => {
                            println!("libp2p: control SendApplyRequest received for peer={}", peer_id);

                            // Create apply request FlatBuffer
                            let operation_id = uuid::Uuid::new_v4().to_string();
                            let manifest_json = manifest.to_string();
                            let local_peer = swarm.local_peer_id().to_string();

                            let apply_request = protocol::machine::build_apply_request(
                                1, // replicas
                                "default", // tenant
                                &operation_id,
                                &manifest_json,
                                &local_peer,
                            );

                            // Send the request via request-response
                            let request_id = swarm.behaviour_mut().apply_rr.send_request(&peer_id, apply_request);
                            println!("libp2p: sent apply request to peer={} request_id={:?}", peer_id, request_id);

                            // For now, just send success immediately - we'll handle proper response tracking later
                            let _ = reply_tx.send(Ok(format!("Apply request sent to {}", peer_id)));
                        }
                    }
                } else {
                    // sender was dropped, exit loop
                    println!("control channel closed");
                    break;
                }
            }
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(MyBehaviourEvent::HandshakeRr(request_response::Event::Message { message, peer, connection_id: _ })) => {
                        match message {
                            request_response::Message::Request { request, channel, .. } => {
                                println!("libp2p: received handshake request from peer={}", peer);

                                // Parse the FlatBuffer handshake request
                                match protocol::machine::root_as_handshake(&request) {
                                    Ok(handshake_req) => {
                                        println!("libp2p: handshake request - signature={:?}", handshake_req.signature());

                                        // Mark this peer as confirmed
                                        let state = handshake_states.entry(peer.clone()).or_insert(HandshakeState {
                                            attempts: 0,
                                            last_attempt: Instant::now() - Duration::from_secs(3),
                                            confirmed: false,
                                        });
                                        state.confirmed = true;

                                        // Create a response with our own signature
                                        //let local_peer_id = swarm.local_peer_id().to_string();
                                        let response = protocol::machine::build_handshake(0, 0, "TODO", "TODO");

                                        // Send the response back
                                        let _ = swarm.behaviour_mut().handshake_rr.send_response(channel, response);
                                        println!("libp2p: sent handshake response to peer={}", peer);
                                    }
                                    Err(e) => {
                                        println!("libp2p: failed to parse handshake request: {:?}", e);
                                        // Send empty response on parse error
                                        let error_response = protocol::machine::build_handshake(0, 0, "TODO", "TODO");
                                        let _ = swarm.behaviour_mut().handshake_rr.send_response(channel, error_response);
                                    }
                                }
                            }
                            request_response::Message::Response { response, .. } => {
                                println!("libp2p: received handshake response from peer={}", peer);

                                // Parse the response
                                match protocol::machine::root_as_handshake(&response) {
                                    Ok(handshake_resp) => {
                                        println!("libp2p: handshake response - signature={:?}", handshake_resp.signature());

                                        // Mark this peer as confirmed
                                        let state = handshake_states.entry(peer.clone()).or_insert(HandshakeState {
                                            attempts: 0,
                                            last_attempt: Instant::now() - Duration::from_secs(3),
                                            confirmed: false,
                                        });
                                        state.confirmed = true;
                                    }
                                    Err(e) => {
                                        println!("libp2p: failed to parse handshake response: {:?}", e);
                                    }
                                }
                            }
                        }
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::HandshakeRr(request_response::Event::OutboundFailure { peer, error, .. })) => {
                        println!("libp2p: handshake outbound failure to peer={}: {:?}", peer, error);
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::HandshakeRr(request_response::Event::InboundFailure { peer, error, .. })) => {
                        println!("libp2p: handshake inbound failure from peer={}: {:?}", peer, error);
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::ApplyRr(request_response::Event::Message { message, peer, connection_id: _ })) => {
                        match message {
                            request_response::Message::Request { request, channel, .. } => {
                                println!("libp2p: received apply request from peer={}", peer);

                                // Parse the FlatBuffer apply request
                                match protocol::machine::root_as_apply_request(&request) {
                                    Ok(apply_req) => {
                                        println!("libp2p: apply request - tenant={:?} operation_id={:?} replicas={}",
                                            apply_req.tenant(), apply_req.operation_id(), apply_req.replicas());

                                        // Create a response (simulate successful apply)
                                        let response = protocol::machine::build_apply_response(
                                            true,
                                            apply_req.operation_id().unwrap_or("unknown"),
                                            "Successfully applied manifest",
                                        );

                                        // Send the response back
                                        let _ = swarm.behaviour_mut().apply_rr.send_response(channel, response);
                                        println!("libp2p: sent apply response to peer={}", peer);
                                    }
                                    Err(e) => {
                                        println!("libp2p: failed to parse apply request: {:?}", e);
                                        let error_response = protocol::machine::build_apply_response(
                                            false,
                                            "unknown",
                                            &format!("Failed to parse request: {:?}", e),
                                        );
                                        let _ = swarm.behaviour_mut().apply_rr.send_response(channel, error_response);
                                    }
                                }
                            }
                            request_response::Message::Response { response, .. } => {
                                println!("libp2p: received apply response from peer={}", peer);

                                // Parse the response
                                match protocol::machine::root_as_apply_response(&response) {
                                    Ok(apply_resp) => {
                                        println!("libp2p: apply response - ok={} operation_id={:?} message={:?}",
                                            apply_resp.ok(), apply_resp.operation_id(), apply_resp.message());
                                    }
                                    Err(e) => {
                                        println!("libp2p: failed to parse apply response: {:?}", e);
                                    }
                                }
                            }
                        }
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::ApplyRr(request_response::Event::OutboundFailure { peer, error, .. })) => {
                        println!("libp2p: outbound request failure to peer={}: {:?}", peer, error);
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::ApplyRr(request_response::Event::InboundFailure { peer, error, .. })) => {
                        println!("libp2p: inbound request failure from peer={}: {:?}", peer, error);
                    }
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
                        let topic = gossipsub::IdentTopic::new(BEEMESH_CLUSTER);
                        let peers: Vec<String> = swarm.behaviour().gossipsub.mesh_peers(&topic.hash()).map(|p| p.to_string()).collect();
                        let _ = peer_tx.send(peers);
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                        for (peer_id, _multiaddr) in list {
                            swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                            handshake_states.remove(&peer_id);
                        }
                        // Update peer list in channel
                        let topic = gossipsub::IdentTopic::new(BEEMESH_CLUSTER);
                        let peers: Vec<String> = swarm.behaviour().gossipsub.mesh_peers(&topic.hash()).map(|p| p.to_string()).collect();
                        let _ = peer_tx.send(peers);
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                        propagation_source: peer_id,
                        message_id: _id,
                        message,
                    })) => {
                        // Try parse the message as a CapacityRequest or CapacityReply (flatbuffers). If that fails, ignore.
                        println!("received message");
                        // First try CapacityRequest
                        if let Ok(cap_req) = protocol::machine::root_as_capacity_request(&message.data) {
                            let orig_request_id = cap_req.request_id().unwrap_or("").to_string();
                            println!("libp2p: received capreq id={} from peer={} payload_bytes={}", orig_request_id, peer_id, message.data.len());
                            // Build a capacity reply and publish it (include request_id inside the reply)
                            let mut fbb = flatbuffers::FlatBufferBuilder::new();
                            let req_id_off = fbb.create_string(&orig_request_id);
                            let node_id_off = fbb.create_string(&peer_id.to_string());
                            let region_off = fbb.create_string("local");
                            // capabilities vector
                            let caps_vec = {
                                let mut tmp: Vec<flatbuffers::WIPOffset<&str>> = Vec::new();
                                tmp.push(fbb.create_string("default"));
                                fbb.create_vector(&tmp)
                            };
                            let reply_args = protocol::machine::CapacityReplyArgs {
                                request_id: Some(req_id_off),
                                ok: true,
                                node_id: Some(node_id_off),
                                region: Some(region_off),
                                capabilities: Some(caps_vec),
                                cpu_available_milli: 1000u32,
                                memory_available_bytes: 1024u64 * 1024 * 512,
                                storage_available_bytes: 1024u64 * 1024 * 1024,
                            };
                            let reply_off = protocol::machine::CapacityReply::create(&mut fbb, &reply_args);
                            protocol::machine::finish_capacity_reply_buffer(&mut fbb, reply_off);
                            let finished = fbb.finished_data().to_vec();
                            let _ = swarm.behaviour_mut().gossipsub.publish(topic.clone(), finished.as_slice());
                            println!("libp2p: published capreply for id={} ({} bytes)", orig_request_id, finished.len());
                            continue;
                        }

                        // Then try CapacityReply
                        if let Ok(cap_reply) = protocol::machine::root_as_capacity_reply(&message.data) {
                            let request_part = cap_reply.request_id().unwrap_or("").to_string();
                            println!("libp2p: received capreply for id={} from peer={}", request_part, peer_id);
                            if let Some(senders) = pending_queries.get_mut(&request_part) {
                                for tx in senders.iter() {
                                    let _ = tx.send(peer_id.to_string());
                                }
                            }
                            continue;
                        }

                        println!("Received non-savvy message ({} bytes) from peer {} — ignoring", message.data.len(), peer_id);
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
                        // Send handshake request using request-response protocol with FlatBuffer
                        let local_peer_id = swarm.local_peer_id().to_string();
                        let handshake_signature = format!("{}-{}", local_peer_id, state.attempts + 1);
                        let handshake_request = protocol::machine::build_handshake(0, 0, "TODO", &handshake_signature);

                        let request_id = swarm.behaviour_mut().handshake_rr.send_request(peer_id, handshake_request);
                        println!("libp2p: sent handshake request to peer={} request_id={:?} attempt={}",
                                peer_id, request_id, state.attempts + 1);

                        state.attempts += 1;
                        state.last_attempt = Instant::now();
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
                let mesh_alive_msg = format!("mesh-alive-{}-{}-{}", swarm.local_peer_id(), now);
                let res = swarm.behaviour_mut().gossipsub.publish(topic.clone(), mesh_alive_msg.as_bytes());
            }*/
        }
    }

    Ok(())
}

/// Control messages sent from the rest API or other parts of the host to the libp2p task.
#[derive(Debug)]
pub enum Libp2pControl {
    QueryCapacityWithPayload {
        request_id: String,
        reply_tx: mpsc::UnboundedSender<String>,
        payload: Vec<u8>,
    },
    SendApplyRequest {
        peer_id: PeerId,
        manifest: serde_json::Value,
        reply_tx: mpsc::UnboundedSender<Result<String, String>>,
    },
}
