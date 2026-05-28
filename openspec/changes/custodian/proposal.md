# Custodian: DEK Share Custody and Key Release

## Purpose

Document the behavior of the custodian subsystem as it exists in the codebase.
This is a descriptive spec — it records observed implementation, not intended design.

## Component Location

`podmesh-scheduler/src/custodian/`

The custodian subsystem runs inside `podmesh-scheduler`. It is activated when the node
starts with `--mode custodian` or `--mode both` (default).

## Actors

- Scheduler (submitter of `WorkloadAssignmentV2`)
- Custodian node (this component)
- Worker node (requester of DEK shares)
- Coordinator (elected custodian; see coordinator election below)
- podctl (originating client, not directly involved at key-release time)

## Observed Behavior

### Coordinator Election

Source: `custodian/coordinator.rs`

- Coordinator is elected per manifest using rendezvous hashing over known custodian peers.
- The elected coordinator is responsible for issuing `WorkloadDispatch` to the worker.
- Coordinator pre-wraps its own DEK share to the worker's KEM public key.

### Share Storage

Source: `custodian/custodian_store.rs`, `storage/custodian_store.rs`

- On receiving a `WorkloadAssignmentV2` via P2P request-response, the custodian stores a
  `CustodianRecord` in a redb-backed on-disk store.
- The record contains: `manifest_id`, `wrapped_share` (ECIES blob), `kfrag_index`,
  `sealed_spec` (ciphertext + metadata), `all_custodian_peers`, `scheduler_sig`.
- The custodian does NOT store the DEK plaintext — only the ECIES-wrapped share.

### Share Release (`ShamirOracle`)

Source: `custodian/oracle_v2.rs`

On receiving a `ShareRequest` from a worker:
1. Verifies the `assignment_sig` is a valid Ed25519 signature by the coordinator
   over `manifest_id || worker_peer_id || assigned_at_secs`.
2. Checks that `assigned_at_secs` is within `ASSIGNMENT_TOKEN_TTL_SECS = 300` seconds of now.
3. Verifies the worker's `ShareRequest.sig` (Ed25519) over canonical request bytes.
4. Looks up the stored `CustodianRecord` for `manifest_id`.
5. Unwraps the stored `wrapped_share` using the custodian's own X25519 KEM private key
   (ECIES: X25519 ECDH + XChaCha20-Poly1305).
6. Re-wraps the raw DEK share to the worker's `worker_kem_pub` using the same ECIES scheme.
7. Signs the response: `custodian_sig = Ed25519.sign(sk, manifest_id || worker_peer_id || wrapped_share)`.
8. Returns `ShareResponse { manifest_id, wrapped_share, custodian_sig }`.

### Heartbeat

Source: `custodian/heartbeat.rs`

- Custodians broadcast `HeartbeatPing` messages on the `podmesh-machine` gossipsub topic.
- `HeartbeatPing` contains: `peer_id`, `timestamp_secs`, `custodian_manifest_ids`, `sig`
  (Ed25519 over canonical bytes).
- Other nodes use these heartbeats to maintain a liveness map of known custodians.
- Current implementation records heartbeat liveness but does not verify heartbeat signatures on receipt.

### CustodianAnnounce / CustodianWithdraw

Source: `podmesh_p2p/behaviour/`

- After storing a share, the custodian broadcasts `CustodianAnnounce { manifest_id, peer_id, ... }`
  on the gossipsub machine topic.
- On deletion or eviction, it broadcasts `CustodianWithdraw`.

## Trust Assumptions (as implemented)

- The custodian trusts the scheduler's `scheduler_sig` on `WorkloadAssignmentV2` for storage.
  There is no check that the scheduler is an authorized node — any peer can claim to be a scheduler.
- The `assignment_sig` in a `ShareRequest` must be signed by the coordinator (rendezvous-elected
  custodian). The custodian resolves the coordinator's signing key from the stored
  `WorkloadAssignmentV2.coordinator_pubkey`.
- There is no certificate chain from coordinator_pubkey to a root of trust.
  The coordinator_pubkey is whatever the scheduler placed in `WorkloadAssignmentV2`.
- The custodian does not verify that `worker_peer_id` in the `ShareRequest` matches the libp2p
  transport identity of the requesting peer.
- TTL enforcement (5 minutes) is the only time-bound protection on the assignment token.

## Data Flow

```
Scheduler
  │ WorkloadAssignmentV2 { sealed_spec, wrapped_kfrag, scheduler_sig, coordinator_pubkey }
  │ (P2P request-response)
  ▼
Custodian stores CustodianRecord in redb
  │ broadcasts CustodianAnnounce on gossipsub
  ▼
Worker (later)
  │ ShareRequest { manifest_id, worker_peer_id, assignment_sig, assigned_at_secs,
  │                worker_kem_pub, nonce, sig }
  │ (P2P request-response)
  ▼
ShamirOracle.release_key_material()
  verify assignment_sig + TTL
  unwrap stored wrapped_kfrag with own KEM priv
  re-wrap raw share to worker_kem_pub
  sign response
  │ ShareResponse { manifest_id, wrapped_share, custodian_sig }
  ▼
Worker
```

## Non-Goals of This Spec

- Does not describe scheduler behavior (see podmesh-scheduler spec)
- Does not describe worker behavior
- Does not describe revocation (not yet implemented)
- Does not describe custodian replication or share recovery on node failure
