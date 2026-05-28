# podmesh-scheduler: Node Orchestration and Workload Routing

## Purpose

Document the behavior of `podmesh-scheduler` as it exists in the codebase.
This is a descriptive spec — it records observed implementation, not intended design.

## Component Location

`podmesh-scheduler/src/`

## Roles

`podmesh-scheduler` is a single binary that runs one or more roles, controlled by `--mode`:

| Mode | Description |
|---|---|
| `worker` | Executes workloads via container/process runtime |
| `custodian` | Stores and releases DEK shares |
| `both` (default) | Performs both roles simultaneously |

The scheduler role (routing, assignment) is always active regardless of mode.

## Interfaces

### REST API (TCP `:3000` by default)

All non-trivial endpoints are wrapped by `envelope_middleware`:
- Decrypts the incoming request body as an `Envelope` (ECIES + Ed25519 signed)
- Extracts `EnvelopeMetadata { peer_id, signing_pubkey, kem_pubkey, original_envelope }`
  and attaches it as an Axum Extension

Key endpoints:
- `GET /health` — unauthenticated health check
- `GET /api/v1/kem_pubkey` — returns node's X25519 KEM public key (unauthenticated)
- `GET /api/v1/signing_pubkey` — returns node's Ed25519 signing public key (unauthenticated)
- `GET /api/v1/custodians?max=N` — discovers custodian peers via libp2p; unauthenticated
- `POST /api/v1/workloads/submit` — accepts `WorkloadSubmission` JSON; verifies `submission_sig`
- `POST /apply_direct/{peer_id}` — forwards an encrypted `ApplyRequest` to a specific peer
- `POST /delete_direct/{peer_id}` — forwards an encrypted `DeleteRequest` to a specific peer
- `GET /runtime/workloads` — lists running workloads
- `GET /runtime/workloads/{id}` — workload detail
- `GET /runtime/workloads/{id}/logs` — workload logs

### Unix Domain Socket (`/run/podmesh/host.sock` by default)

Host-side API for local agents (e.g. sidecar injector). Defined in `hostapi/mod.rs`.

### libp2p (QUIC, auto-assigned port)

Protocols served or consumed:
- Kademlia DHT (server mode): peer/record discovery
- gossipsub: `podmesh-workload` and `podmesh-machine` topics
- `HandshakeProtocol`: key exchange with peers
- `WorkloadAssignmentV2` request-response: send to custodians
- `WorkloadDispatch` request-response: send to workers
- `ShareRequest/ShareResponse`: key release (custodian role)
- `ApplyRequest/ApplyResponse`: deploy manifest to worker
- `DeleteRequest/DeleteResponse`: remove workload from worker

## Observed Behavior

### Workload Submission

Source: `restapi/mod.rs`, `lib.rs`, `podmesh_p2p/`

1. Receives `WorkloadSubmission` JSON at `POST /api/v1/workloads/submit`.
2. Verifies `submission_sig` (Ed25519, signer = `owner_pubkey`, over `postcard(sealed_spec)`).
3. For each `SubmittedShare` in `wrapped_shares`, sends `WorkloadAssignmentV2` to the named
   custodian peer via P2P request-response.
4. `WorkloadAssignmentV2` contains: `sealed_spec`, `wrapped_kfrag`, `kfrag_index`,
   `scheduler_sig` (signed by scheduler's own Ed25519 key), `coordinator_pubkey`
   (the rendezvous-elected custodian's pubkey), `all_custodian_peers`.
5. Returns `WorkloadSubmissionResponse { manifest_id, custodians_assigned, custodian_peers }`.

### Worker Selection

Source: `scheduler.rs`, `podmesh_p2p/behaviour/`

- Broadcasts `CapabilityQuery` on gossipsub `podmesh-workload` topic.
- Collects `CapabilityReply` responses (include peer KEM pubkey and capabilities).
- Optionally sends `ResourceQuery` to candidates to check available cpu/mem/storage.
- Selects worker(s) via round-robin or load-based selection.
- Coordinator (elected by rendezvous hash among custodians) builds `WorkloadDispatch` and sends
  to the selected worker via P2P request-response.

### Runtime

Source: `runtime/`

- `RuntimeEngine` trait: `deploy`, `status`, `delete`, `logs`, `list`.
- Implementations: `PodmanEngine` (Podman socket API), `ProcessEngine` (raw process exec).
- Registered engines exposed via `GET /runtime/engines`.
- `--mock-only-runtime` substitutes a `MockEngine` for testing.

### Sidecar Injection

Source: `workload_manager.rs`, `workload_integration.rs`

- After workload deployment, the scheduler writes `SidecarMetadata` to
  `/var/run/podmesh/sidecar/metadata.json` inside the container namespace.
- `SidecarMetadata` includes: `manifest_id`, `manifest_b64`, `owner_public_key_b64`,
  `bootstrap_peer` (multiaddr for the sidecar's DHT bootstrap).
- Sidecar image is configured via `--sidecar-image`.

## Trust Assumptions (as implemented)

- `submission_sig` is verified — the scheduler confirms the submitter holds the owner private key.
- The scheduler does not verify that named custodian peers in `wrapped_shares` are legitimate
  custodians — it routes to whatever peer_id the client specifies.
- The `scheduler_sig` placed in `WorkloadAssignmentV2` uses the scheduler's own keypair.
  Custodians store it but the current oracle_v2 does not verify the scheduler's identity
  against any allowlist.
- REST endpoints behind the envelope middleware require a valid ECIES-wrapped envelope,
  but the signer is not checked against a trusted-node list.
- `GET /api/v1/custodians` and `GET /api/v1/kem_pubkey` are fully unauthenticated.

## Data Flow

```
podctl
  │ POST /api/v1/workloads/submit [WorkloadSubmission JSON]
  ▼
scheduler verifies submission_sig
  │ for each SubmittedShare:
  │   P2P RR → custodian: WorkloadAssignmentV2
  ▼
custodians store shares
  │ gossipsub: CapabilityQuery (podmesh-workload topic)
  ▼
worker nodes reply: CapabilityReply
  │ scheduler selects worker
  │ coordinator builds WorkloadDispatch
  │ P2P RR → worker: WorkloadDispatch
  ▼
worker collects shares, unseals, executes workload
```

## Non-Goals of This Spec

- Does not describe custodian internals (see custodian spec)
- Does not describe worker decryption (see workload-execution spec)
- Does not describe sidecar or proxy behavior
