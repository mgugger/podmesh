# Custodian

## ADDED Requirements

### Requirement: Custodian stores ECIES-wrapped DEK shares

The custodian MUST persist wrapped DEK shares received via P2P without decrypting them.

#### Scenario: Custodian receives WorkloadAssignmentV2
- Given a custodian node is running in custodian or both mode
- And a scheduler sends a `WorkloadAssignmentV2` via P2P request-response
- When the custodian processes the assignment
- Then it stores a `CustodianRecord` (manifest_id, wrapped_kfrag, kfrag_index, sealed_spec) in redb
- And it broadcasts `CustodianAnnounce` on the `podmesh-machine` gossipsub topic
- And it does NOT decrypt the wrapped_kfrag

### Requirement: Custodian releases shares only to token-bearing workers

The custodian MUST verify a coordinator-issued assignment token before releasing a DEK share.

#### Scenario: Worker requests a DEK share
- Given a `CustodianRecord` exists for the requested manifest_id
- And a worker sends a `ShareRequest` containing an assignment_sig and assigned_at_secs
- When `ShamirOracle.release_key_material()` is called
- Then it verifies the assignment_sig is an Ed25519 signature by coordinator_pubkey over `manifest_id || worker_peer_id || assigned_at_secs`
- And it checks that `now - assigned_at_secs <= 300 seconds`
- And it verifies the worker's own Ed25519 sig over the request canonical bytes
- And it unwraps the stored share with the custodian's X25519 KEM private key
- And it re-wraps the raw share to the worker's `worker_kem_pub`
- And it returns `ShareResponse { manifest_id, wrapped_share, custodian_sig }`

### Requirement: Custodian broadcasts liveness via heartbeat

Custodians MUST periodically broadcast a signed heartbeat for liveness tracking.

#### Scenario: Custodian heartbeat broadcast
- Given a node is operating as a custodian
- When the heartbeat timer fires
- Then a `HeartbeatPing` is published on the `podmesh-machine` gossipsub topic
- And the ping contains: `peer_id`, `timestamp_secs`, `custodian_manifest_ids`, `sig`

## OBSERVED Trust Gaps (not requirements — observations)

### Observation: Scheduler identity is not verified on assignment receipt

The custodian accepts `WorkloadAssignmentV2` from any peer that can reach it via P2P.
The `scheduler_sig` field is stored but there is no check that the signing key belongs
to a trusted scheduler node. Any peer can submit a crafted assignment.

### Observation: Worker peer_id is not verified against transport identity

The `ShareRequest.worker_peer_id` field is accepted as provided in the request body.
The custodian does not verify that the libp2p connection's remote peer ID matches
the claimed `worker_peer_id`.

### Observation: Heartbeat signatures are emitted but not verified on receipt

Heartbeat messages are signed by senders, but current liveness tracking records
incoming heartbeats without verifying `sig` against a trusted key.
