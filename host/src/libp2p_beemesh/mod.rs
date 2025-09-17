use futures::stream::StreamExt;
use libp2p::{
    gossipsub, mdns, noise, request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId, Swarm,
};
use std::{
    collections::hash_map::DefaultHasher,
    error::Error,
    hash::{Hash, Hasher},
    time::Duration,
};
use tokio::sync::{watch, mpsc};
use std::collections::HashMap as StdHashMap;

use beemesh_protocol::libp2p_constants::{BEEMESH_CLUSTER, BINARY_ENVELOPE_VERSION};

// Compact binary envelope magic bytes for capacity request/reply (avoids JSON+base64).
const CAPREQ_MAGIC: u8 = 0xC1;
const CAPREPLY_MAGIC: u8 = 0xC2;

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
fn encode_varint(mut mut_v: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    loop {
        let byte = (mut_v & 0x7F) as u8;
        mut_v >>= 7;
        if mut_v != 0 {
            buf.push(byte | 0x80);
        } else {
            buf.push(byte);
            break;
        }
    }
    buf
}

fn decode_varint(buf: &[u8]) -> Option<(usize, usize)> {
    let mut result: usize = 0;
    let mut shift = 0;
    for (i, &b) in buf.iter().enumerate() {
        result |= ((b & 0x7F) as usize) << shift;
        if (b & 0x80) == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
        // safety: cap at 64 bits
        if shift > 63 {
            return None;
        }
    }
    None
}

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
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            || yamux::Config::default(),
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
                std::iter::once(("/beemesh/apply/1.0.0", request_response::ProtocolSupport::Full)),
                request_response::Config::default(),
            );
            
            // Create the request-response behavior for handshake protocol
            let handshake_rr = request_response::Behaviour::new(
                std::iter::once(("/beemesh/handshake/1.0.0", request_response::ProtocolSupport::Full)),
                request_response::Config::default(),
            );
            
            Ok(MyBehaviour { gossipsub, mdns, apply_rr, handshake_rr })
        })?
        .build();

    let topic = gossipsub::IdentTopic::new(BEEMESH_CLUSTER);
    println!("Subscribing to topic: {}", topic.hash());
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;
    // Ensure local host is an explicit mesh peer for the topic so publish() finds at least one subscriber
    let local_peer = swarm.local_peer_id().clone();
    swarm.behaviour_mut().gossipsub.add_explicit_peer(&local_peer);

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
    let mut pending_queries: StdHashMap<String, Vec<mpsc::UnboundedSender<String>>> = StdHashMap::new();

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
                            // publish the query to the cluster as a compact binary envelope:
                            // [MAGIC][varint: id_len][id bytes][flatbuffer bytes]
                            let id_bytes = request_id.as_bytes();
                            let id_len_varint = encode_varint(id_bytes.len());
                            let mut envelope: Vec<u8> = Vec::with_capacity(1 + 1 + id_len_varint.len() + id_bytes.len() + payload.len());
                            envelope.push(CAPREQ_MAGIC);
                            envelope.push(BINARY_ENVELOPE_VERSION);
                            envelope.extend_from_slice(&id_len_varint);
                            envelope.extend_from_slice(id_bytes);
                            envelope.extend_from_slice(&payload);
                            let res = swarm.behaviour_mut().gossipsub.publish(topic.clone(), envelope.as_slice());
                            println!("libp2p: published capreq request_id={} publish_res={:?}", request_id, res);
                            // Also notify local pending senders directly so the originator is always considered
                            // a potential responder. This ensures single-node operation and makes the
                            // origining host countable when collecting responders.
                            if let Some(senders) = pending_queries.get_mut(&request_id) {
                                for tx in senders.iter() {
                                    let _ = tx.send(swarm.local_peer_id().to_string());
                                }
                            }
                            // legacy textual publish removed — all peers are upgraded
                        }
                        Libp2pControl::SendApplyRequest { peer_id, manifest, reply_tx } => {
                            println!("libp2p: control SendApplyRequest received for peer={}", peer_id);
                            
                            // Create apply request FlatBuffer
                            let operation_id = uuid::Uuid::new_v4().to_string();
                            let manifest_json = manifest.to_string();
                            let local_peer = swarm.local_peer_id().to_string();
                            
                            let apply_request = beemesh_protocol::flatbuffer::build_apply_request(
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
                                match beemesh_protocol::flatbuffer::root_as_handshake(&request) {
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
                                        let local_peer_id = swarm.local_peer_id().to_string();
                                        let response = beemesh_protocol::flatbuffer::build_handshake(0, 0, "TODO", "TODO");
                                        
                                        // Send the response back
                                        let _ = swarm.behaviour_mut().handshake_rr.send_response(channel, response);
                                        println!("libp2p: sent handshake response to peer={}", peer);
                                    }
                                    Err(e) => {
                                        println!("libp2p: failed to parse handshake request: {:?}", e);
                                        // Send empty response on parse error
                                        let error_response = beemesh_protocol::flatbuffer::build_handshake(0, 0, "TODO", "TODO");
                                        let _ = swarm.behaviour_mut().handshake_rr.send_response(channel, error_response);
                                    }
                                }
                            }
                            request_response::Message::Response { response, .. } => {
                                println!("libp2p: received handshake response from peer={}", peer);
                                
                                // Parse the response
                                match beemesh_protocol::flatbuffer::root_as_handshake(&response) {
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
                                match beemesh_protocol::flatbuffer::root_as_apply_request(&request) {
                                    Ok(apply_req) => {
                                        println!("libp2p: apply request - tenant={:?} operation_id={:?} replicas={}",
                                            apply_req.tenant(), apply_req.operation_id(), apply_req.replicas());
                                        
                                        // Create a response (simulate successful apply)
                                        let response = beemesh_protocol::flatbuffer::build_apply_response(
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
                                        let error_response = beemesh_protocol::flatbuffer::build_apply_response(
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
                                match beemesh_protocol::flatbuffer::root_as_apply_response(&response) {
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
                        // First try compact binary envelope parsing (fast, no copying/base64)
                        println!("received message");
                        if message.data.len() >= 3 {
                            let magic = message.data[0];
                            if magic == CAPREQ_MAGIC {
                                // compact binary capreq: [MAGIC][VERSION][varint:id_len][id bytes][payload]
                                if message.data.len() >= 2 {
                                    let version = message.data[1];
                                    if version != BINARY_ENVELOPE_VERSION {
                                        println!("Ignoring envelope with unsupported version: {}", version);
                                    } else if let Some((id_len, varint_len)) = decode_varint(&message.data[2..]) {
                                        let start = 2 + varint_len;
                                        if message.data.len() >= start + id_len {
                                            let id_bytes = &message.data[start..start + id_len];
                                            // id_bytes contains the original request id
                                            let orig_request_id = String::from_utf8_lossy(id_bytes).to_string();
                                            println!("libp2p: received capreq id={} from peer={} payload_bytes={}", orig_request_id, peer_id, message.data.len());
                                            // build reply flatbuffer and publish as binary capreply envelope over gossipsub
                                            let reply_fb = beemesh_protocol::flatbuffer::build_capacity_reply(
                                                true,
                                                1000,
                                                1024 * 1024 * 512, // 512MB
                                                1024 * 1024 * 1024, // 1GB
                                                &peer_id.to_string(),
                                                "local",
                                                & ["default"],
                                            );
                                            let id_len_varint = encode_varint(orig_request_id.len());
                                            let mut reply_envelope: Vec<u8> = Vec::with_capacity(1 + 1 + id_len_varint.len() + orig_request_id.len() + reply_fb.len());
                                            reply_envelope.push(CAPREPLY_MAGIC);
                                            reply_envelope.push(BINARY_ENVELOPE_VERSION);
                                            reply_envelope.extend_from_slice(&id_len_varint);
                                            reply_envelope.extend_from_slice(orig_request_id.as_bytes());
                                            reply_envelope.extend_from_slice(&reply_fb);
                                            let _ = swarm.behaviour_mut().gossipsub.publish(topic.clone(), reply_envelope.as_slice());
                                            println!("libp2p: published capreply for id={} ({} bytes)", orig_request_id, reply_fb.len());
                                            let _ = swarm.behaviour_mut().gossipsub.publish(topic.clone(), reply_envelope.as_slice());
                                            continue;
                                        }
                                    }
                                }
                            } else if magic == CAPREPLY_MAGIC {
                                // parse varint id length and forward reply to pending queries
                                if message.data.len() >= 2 {
                                    if let Some((id_len, varint_len)) = decode_varint(&message.data[2..]) {
                                        if message.data.len() >= 2 + varint_len + id_len {
                                            let start = 2 + varint_len;
                                            let id_bytes = &message.data[start..start+id_len];
                                            let request_part = String::from_utf8_lossy(id_bytes).to_string();
                                            println!("libp2p: received capreply for id={} from peer={}", request_part, peer_id);
                                            if let Some(senders) = pending_queries.get_mut(&request_part) {
                                                for tx in senders.iter() {
                                                    let _ = tx.send(peer_id.to_string());
                                                }
                                            }
                                            continue;
                                        }
                                    }
                                }
                            }
                        }

                        let msg_content = String::from_utf8_lossy(&message.data);
                        let _s = msg_content.as_ref();

                        // All non-binary messages are now ignored since we use request-response for handshake
                        println!(
                            "Received non-binary message ({} bytes) from peer {} — ignoring",
                            message.data.len(), peer_id
                        );
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
                        let handshake_request = beemesh_protocol::flatbuffer::build_handshake(0, 0, "TODO", &handshake_signature);
                        
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
