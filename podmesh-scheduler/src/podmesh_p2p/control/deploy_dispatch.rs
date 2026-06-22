//! Worker-side share collection, unsealing, and deployment (Shamir secret sharing).
//!
//! After the coordinator sends a `WorkloadDispatch`, this handler:
//! 1. Sends a `ShareRequest` to each custodian over `scheduler_rr`.
//! 2. Collects at least `threshold` `ShareResponse` replies (each containing a wrapped DEK share).
//! 3. Calls `unseal_spec` (Shamir reconstruct + decrypt) to recover the plaintext spec.
//! 4. Applies the workload via the local runtime engine.

use libp2p::Swarm;
use log::{error, info, warn};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::mpsc;

use crypto::{ShareRequest, ShareResponse, b64_encode};
use protocol::machine::WorkloadDispatch;
use crate::podmesh_p2p::behaviour::MyBehaviour;
use crate::podmesh_p2p::utils;
use crate::custodian::sealer::unseal_spec;

// ---------------------------------------------------------------------------
// Pending share-request tracking
// ---------------------------------------------------------------------------

/// In-flight share requests: "{local_peer_id}:{OutboundRequestId}" → reply sender.
/// The local_peer_id prefix ensures in-process test nodes with separate scheduler_rr
/// instances (whose OutboundRequestIds both start from 0) don't collide.
static PENDING_SHARE_REQUESTS: Lazy<Mutex<HashMap<String, mpsc::UnboundedSender<Option<ShareResponse>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn insert_pending_share_request(
    local_peer_id: &str,
    request_id: &libp2p::request_response::OutboundRequestId,
    tx: mpsc::UnboundedSender<Option<ShareResponse>>,
) {
    let key = format!("{}:{:?}", local_peer_id, request_id);
    log::warn!("insert_pending_share_request: key={}", key);
    PENDING_SHARE_REQUESTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, tx);
}

/// Called when an OutboundFailure occurs for a request that may be a ShareRequest.
/// Signals the waiting task with `None` so it can fail fast instead of timing out.
pub fn notify_share_request_failed(
    local_peer_id: &str,
    request_id: &libp2p::request_response::OutboundRequestId,
) {
    let key = format!("{}:{:?}", local_peer_id, request_id);
    let tx = PENDING_SHARE_REQUESTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&key);
    if let Some(tx) = tx {
        let _ = tx.send(None);
    }
}

/// Called from `handle_scheduler_response` when a response arrives.
/// Returns `true` if the response was consumed as a ShareResponse.
pub fn try_deliver_share_response(
    local_peer_id: &str,
    request_id: &libp2p::request_response::OutboundRequestId,
    response: &[u8],
) -> bool {
    let key = format!("{}:{:?}", local_peer_id, request_id);
    log::debug!("try_deliver_share_response: key={} response_len={}", key, response.len());
    let tx = {
        PENDING_SHARE_REQUESTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key)
    };
    if let Some(tx) = tx {
        if response.is_empty() {
            let _ = tx.send(None);
        } else if let Ok(resp) = ShareResponse::from_bytes(response) {
            let _ = tx.send(Some(resp));
        } else {
            let parse_err = postcard::from_bytes::<ShareResponse>(response).unwrap_err();
            log::warn!("try_deliver_share_response: failed to parse ShareResponse ({} bytes) for key={}: {:?} ALL_BYTES={:?}", response.len(), key, parse_err, response);
            let _ = tx.send(None);
        }
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Main handler
// ---------------------------------------------------------------------------

const SHARE_COLLECTION_TIMEOUT_MS: u64 = 10_000;

pub async fn handle_deploy_dispatched_workload(
    dispatch: WorkloadDispatch,
    worker_peer_id: libp2p::PeerId,
    swarm: &mut Swarm<MyBehaviour>,
) {
    let manifest_id = dispatch.sealed_spec.manifest_id.clone();
    let threshold = dispatch.sealed_spec.kfrag_threshold as usize;
    let assignment_token = dispatch.assignment_token.clone();
    let assigned_at_secs = dispatch.assigned_at_secs;
    let coordinator_peer_id = dispatch.coordinator_peer_id.clone();

    info!(
        "deploy_dispatch: manifest_id={} threshold={} pre-wrapped-shares={}",
        manifest_id, threshold, dispatch.worker_wrapped_shares.len()
    );

    // Seed wrapped_cfrags with any shares the coordinator pre-wrapped for us.
    let pre_wrapped: Vec<Vec<u8>> = dispatch.worker_wrapped_shares.clone();

    if pre_wrapped.len() >= threshold {
        // Fast path: coordinator supplied all required shares.
        // Spawn a separate task so we don't block the swarm event loop.
        let sealed_spec = dispatch.sealed_spec.clone();
        tokio::spawn(async move {
            let (_, kem_priv) = match crypto::ensure_kem_keypair_on_disk() {
                Ok(kp) => kp,
                Err(e) => {
                    error!("deploy_dispatch: KEM keypair unavailable: {}", e);
                    return;
                }
            };
            let spec_json = match unseal_spec(&sealed_spec, &pre_wrapped, &kem_priv) {
                Ok(s) => s,
                Err(e) => {
                    error!("deploy_dispatch: unseal_spec failed for manifest_id={}: {}", manifest_id, e);
                    return;
                }
            };
            info!(
                "deploy_dispatch: unsealed spec for manifest_id={} ({} bytes) via fast path",
                manifest_id, spec_json.len()
            );
            deploy_spec(&manifest_id, &spec_json, &worker_peer_id).await;
        });
        return;
    }

    // Slow path: issue ShareRequests synchronously (needs swarm), then spawn
    // a task to wait for replies and do the unseal + deploy.
    let custodians = dispatch.custodian_peers.clone();
    info!(
        "deploy_dispatch: manifest_id={} have {}/{} shares, requesting remainder from {} custodians",
        manifest_id, pre_wrapped.len(), threshold, custodians.len()
    );

    // --- Build our worker identity ---
    let local_peer_id = worker_peer_id.to_string();
    let worker_kem_pub_bytes = match crypto::ensure_kem_keypair_on_disk() {
        Ok((pub_bytes, _)) => pub_bytes,
        Err(e) => {
            error!("deploy_dispatch: failed to load KEM keypair: {}", e);
            return;
        }
    };
    let worker_kem_pub_b64 = b64_encode(&worker_kem_pub_bytes);
    let (_, worker_signing_priv) = match crypto::ensure_keypair_on_disk() {
        Ok(kp) => kp,
        Err(e) => {
            error!("deploy_dispatch: failed to load signing keypair: {}", e);
            return;
        }
    };

    // --- Issue ShareRequests (sync, needs swarm) and collect local shares immediately ---
    let mut wrapped_cfrags: Vec<Vec<u8>> = pre_wrapped;
    let mut reply_rxs: Vec<mpsc::UnboundedReceiver<Option<ShareResponse>>> = Vec::new();

    info!(
        "deploy_dispatch: local_peer_id={} coordinator={} custodians={:?}",
        local_peer_id, coordinator_peer_id, custodians
    );

    for (custodian_idx, custodian_peer_id_str) in custodians.iter().enumerate() {
        if wrapped_cfrags.len() >= threshold {
            break;
        }

        // Skip the coordinator: it already pre-wrapped its share into worker_wrapped_shares.
        if !coordinator_peer_id.is_empty() && custodian_peer_id_str == &coordinator_peer_id {
            info!(
                "deploy_dispatch: skipping coordinator peer {} (share already pre-wrapped)",
                custodian_peer_id_str
            );
            continue;
        }

        // Local custodian: fetch share directly from the CustodianStore and re-wrap.
        if custodian_peer_id_str == &local_peer_id {
            match serve_local_share(&manifest_id, &worker_kem_pub_bytes, &local_peer_id) {
                Some(wrapped) => {
                    info!(
                        "deploy_dispatch: served local share for manifest_id={}",
                        manifest_id
                    );
                    wrapped_cfrags.push(wrapped);
                }
                None => {
                    warn!("deploy_dispatch: local share not found for manifest_id={}", manifest_id);
                }
            }
            continue;
        }

        let custodian_peer_id = match custodian_peer_id_str.parse::<libp2p::PeerId>() {
            Ok(id) => id,
            Err(e) => {
                warn!("deploy_dispatch: invalid peer_id '{}': {}", custodian_peer_id_str, e);
                continue;
            }
        };

        // If this custodian's address is known (via Identify), log it for debugging.
        if let Some(addr) = crate::podmesh_p2p::get_peer_address(&custodian_peer_id) {
            info!(
                "deploy_dispatch: known address for custodian {} = {} (manifest_id={})",
                custodian_peer_id, addr, manifest_id
            );
        } else {
            warn!(
                "deploy_dispatch: no known address for custodian {} (manifest_id={})",
                custodian_peer_id, manifest_id
            );
        }

        let nonce = b64_encode(&utils::make_nonce(Some("sr")).as_bytes().to_vec());
        let node_cert_bytes = load_local_node_cert_bytes_for_peer(&local_peer_id);
        let tenant_owner_pubkey_b64 = protocol::NodeCert::from_bytes(&node_cert_bytes)
            .map(|c| c.owner_pubkey)
            .unwrap_or_default();
        let now_unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let share_index = (custodian_idx as u32) + 1;
        let authz_ctx = protocol::AuthzContext {
            tenant_owner_pubkey_b64,
            manifest_id: manifest_id.clone(),
            transport_peer_id: local_peer_id.clone(),
            operation: protocol::AuthzOperation::ReleaseShare,
            http_path: None,
            dest_host: None,
            dest_port: None,
            worker_peer_id: Some(local_peer_id.clone()),
            share_index: Some(share_index),
            delegate_peer_id: None,
            now_unix_secs,
        };
        let authz_token_b64 = match protocol::mint_release_share_token_b64(&worker_signing_priv, &authz_ctx) {
            Ok(t) => Some(t),
            Err(e) => {
                warn!("deploy_dispatch: failed to mint release_share authz token: {}", e);
                continue;
            }
        };

        let mut req = ShareRequest {
            manifest_id: manifest_id.clone(),
            worker_peer_id: local_peer_id.clone(),
            node_cert_bytes,
            assignment_sig: assignment_token.clone(),
            assigned_at_secs,
            share_index: Some(share_index),
            authz_token_b64,
            worker_kem_pub: worker_kem_pub_b64.clone(),
            nonce,
            sig: String::new(),
        };

        // Sign the request
        let canonical = req.canonical_bytes();
        match crypto::sign_data_with_key(&worker_signing_priv, &canonical) {
            Ok(sig_bytes) => req.sig = b64_encode(&sig_bytes),
            Err(e) => {
                warn!("deploy_dispatch: failed to sign ShareRequest: {}", e);
                continue;
            }
        }

        let req_bytes = req.to_bytes();
        let signed_req = match utils::sign_payload_default(&req_bytes, "share_request", Some("sreq")) {
            Ok(s) => s,
            Err(e) => {
                warn!("deploy_dispatch: failed to sign ShareRequest envelope: {}", e);
                continue;
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let out_id = swarm.behaviour_mut().scheduler_rr.send_request(&custodian_peer_id, signed_req);
        insert_pending_share_request(&local_peer_id, &out_id, tx);
        reply_rxs.push(rx);
    }

    // Spawn a separate task to wait for remote replies (so swarm event loop is NOT blocked).
    let sealed_spec = dispatch.sealed_spec.clone();
    tokio::spawn(async move {
        let mut wrapped_cfrags = wrapped_cfrags;

        // --- Wait for remote share replies if needed ---
        if !reply_rxs.is_empty() && wrapped_cfrags.len() < threshold {
            let deadline = tokio::time::Instant::now()
                + tokio::time::Duration::from_millis(SHARE_COLLECTION_TIMEOUT_MS);

            for mut rx in reply_rxs {
                if wrapped_cfrags.len() >= threshold {
                    break;
                }
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(Some(resp))) => {
                        wrapped_cfrags.push(resp.wrapped_share);
                        info!(
                            "deploy_dispatch: collected remote share {}/{} for manifest_id={}",
                            wrapped_cfrags.len(), threshold, manifest_id
                        );
                    }
                    Ok(Some(None)) => warn!("deploy_dispatch: custodian returned empty ShareResponse"),
                    Ok(None) | Err(_) => {
                        warn!("deploy_dispatch: timed out waiting for remote share");
                        break;
                    }
                }
            }
        }

        if wrapped_cfrags.is_empty() {
            error!("deploy_dispatch: no shares collected for manifest_id={}", manifest_id);
            return;
        }

        if wrapped_cfrags.len() < threshold {
            error!(
                "deploy_dispatch: only {}/{} shares collected for manifest_id={}, aborting",
                wrapped_cfrags.len(), threshold, manifest_id
            );
            return;
        }

        // --- Load worker KEM keypair ---
        let (_, kem_priv) = match crypto::ensure_kem_keypair_on_disk() {
            Ok(kp) => kp,
            Err(e) => {
                error!("deploy_dispatch: KEM keypair unavailable: {}", e);
                return;
            }
        };

        // --- Unseal the spec ---
        let spec_json = match unseal_spec(&sealed_spec, &wrapped_cfrags, &kem_priv) {
            Ok(s) => s,
            Err(e) => {
                error!("deploy_dispatch: unseal_spec failed for manifest_id={}: {}", manifest_id, e);
                return;
            }
        };

        info!(
            "deploy_dispatch: unsealed spec for manifest_id={} ({} bytes)",
            manifest_id, spec_json.len()
        );

        // --- Deploy via engine ---
        deploy_spec(&manifest_id, &spec_json, &worker_peer_id).await;
    });
}

/// Serve a DEK share for the local custodian directly from `CustodianStore`.
/// Decrypts the stored wrapped share with the local KEM private key, then
/// re-encrypts it for the worker's KEM public key.
fn serve_local_share(manifest_id: &str, worker_kem_pub: &[u8], local_peer_id: &str) -> Option<Vec<u8>> {
    let store = crate::storage::get_custodian_store()?;

    let (_, kem_priv) = crypto::ensure_kem_keypair_on_disk().ok()?;

    // Use peer-scoped record lookup if possible, fall back to bare manifest_id.
    let record = store.get_record_for_peer(manifest_id, local_peer_id)
        .ok()
        .flatten()
        .or_else(|| store.get_record(manifest_id).ok().flatten())?;

    // Decrypt the wrapped share using the local custodian's KEM private key.
    let raw_share = crypto::decrypt_payload_from_recipient_blob(&record.wrapped_share, &kem_priv)
        .map_err(|e| {
            warn!("serve_local_share: failed to decrypt share for {}: {}", manifest_id, e);
            e
        })
        .ok()?;

    // Re-wrap the raw share for the worker's KEM public key.
    let re_wrapped = crypto::encrypt_payload_for_recipient(worker_kem_pub, &raw_share)
        .map_err(|e| {
            warn!("serve_local_share: failed to re-wrap share for {}: {}", manifest_id, e);
            e
        })
        .ok()?;

    Some(re_wrapped)
}

/// Apply the plaintext spec JSON via the workload manager / engine.
async fn deploy_spec(manifest_id: &str, spec_json: &str, local_peer_id: &libp2p::PeerId) {
    info!("deploy_dispatch: deploying manifest_id={} as peer={}", manifest_id, local_peer_id);

    let registry_opt = crate::workload_integration::get_global_runtime_registry().await;
    let registry = match registry_opt.as_ref() {
        Some(guard) => match guard.as_ref() {
            Some(r) => r,
            None => {
                warn!("deploy_dispatch: runtime registry not initialized, skipping deploy");
                return;
            }
        },
        None => {
            warn!("deploy_dispatch: runtime registry not available, skipping deploy");
            return;
        }
    };

    let engine = match registry.get_default_engine() {
        Some(e) => e,
        None => {
            warn!("deploy_dispatch: no default engine available");
            return;
        }
    };

    let config = crate::runtime::DeploymentConfig::default();
    match engine.deploy_workload_with_peer(manifest_id, spec_json.as_bytes(), &config, *local_peer_id).await {
        Ok(_) => info!("deploy_dispatch: deployed manifest_id={}", manifest_id),
        Err(e) => error!("deploy_dispatch: deploy failed for {}: {}", manifest_id, e),
    }
}

pub(crate) fn load_local_node_cert_bytes_for_peer(local_peer_id: &str) -> Vec<u8> {
    if let Ok(home) = std::env::var("HOME") {
        let key_dir = std::path::PathBuf::from(home).join(crypto::KEY_DIR);
        if let Ok(Some(cert)) = protocol::node_cert::load_node_cert(key_dir.to_str().unwrap_or(".podmesh")) {
            return cert.to_bytes();
        }
    }

    // Ephemeral mode fallback: construct an in-memory self-signed NodeCert from
    // the currently configured signing/KEM keypairs so authz-bound ShareRequest
    // validation still has a concrete worker identity.
    let (signing_pub, signing_priv) = match crypto::ensure_keypair_on_disk() {
        Ok(kp) => kp,
        Err(e) => {
            warn!("load_local_node_cert_bytes_for_peer: failed to load signing keypair: {}", e);
            return vec![];
        }
    };
    let (kem_pub, _kem_priv) = match crypto::ensure_kem_keypair_on_disk() {
        Ok(kp) => kp,
        Err(e) => {
            warn!("load_local_node_cert_bytes_for_peer: failed to load KEM keypair: {}", e);
            return vec![];
        }
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let cert = protocol::NodeCert {
        peer_id: local_peer_id.to_string(),
        kem_pubkey: crypto::b64_encode(&kem_pub),
        signing_pubkey: crypto::b64_encode(&signing_pub),
        capabilities: vec!["default".to_string()],
        role: protocol::NodeRole::Both,
        valid_until: now.saturating_add(24 * 60 * 60),
        owner_pubkey: crypto::b64_encode(&signing_pub),
        owner_sig: String::new(),
        endorsements: vec![],
    };

    match cert.sign(&signing_priv, &signing_pub) {
        Ok(signed) => signed.to_bytes(),
        Err(e) => {
            warn!("load_local_node_cert_bytes_for_peer: failed to self-sign fallback NodeCert: {}", e);
            vec![]
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_deliver_share_response_empty_returns_none() {
        let fake_id_key = "__test_empty__".to_string();
        let (tx, mut rx) = mpsc::unbounded_channel::<Option<ShareResponse>>();
        PENDING_SHARE_REQUESTS
            .lock()
            .unwrap()
            .insert(fake_id_key.clone(), tx);

        let found = {
            PENDING_SHARE_REQUESTS
                .lock()
                .unwrap()
                .remove(&fake_id_key)
        };
        if let Some(tx) = found {
            let _ = tx.send(None);
        }

        let result = rx.try_recv().unwrap();
        assert!(result.is_none());
    }
}
