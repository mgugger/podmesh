use libp2p::request_response;

pub fn apply_message(
    message: request_response::Message<Vec<u8>, Vec<u8>>,
    peer: libp2p::PeerId,
    swarm: &mut libp2p::Swarm<super::MyBehaviour>,
) {
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
