# podmesh-scheduler

## ADDED Requirements

### Requirement: Scheduler verifies workload submission signature

The scheduler MUST verify the owner's Ed25519 signature before processing a `WorkloadSubmission`.

#### Scenario: Valid workload submission received
- Given a `WorkloadSubmission` is POSTed to `/api/v1/workloads/submit`
- When the scheduler processes it
- Then it verifies `submission_sig` is an Ed25519 signature by `sealed_spec.owner_pubkey`
  over `postcard(sealed_spec)`
- And it proceeds to route shares to custodians only if the signature is valid

### Requirement: Scheduler routes DEK shares to custodian peers without decrypting

The scheduler MUST forward ECIES-wrapped DEK shares to named custodian peers opaquely.

#### Scenario: Scheduler distributes WorkloadAssignmentV2 to custodians
- Given a verified `WorkloadSubmission` with N `SubmittedShare` entries
- When the scheduler processes each share
- Then it sends `WorkloadAssignmentV2` via P2P request-response to the `custodian_peer_id`
  named in that share
- And the `wrapped_kfrag` is forwarded as-is, without decryption
- And the scheduler adds its own `scheduler_sig` (Ed25519 over `postcard(sealed_spec)`)
  and `coordinator_pubkey` (rendezvous-elected custodian) to the assignment

### Requirement: Scheduler exposes key material discovery endpoints without authentication

The scheduler MUST expose KEM and signing public keys and custodian lists to unauthenticated callers.

#### Scenario: Client discovers custodians before submission
- Given the scheduler is running
- When a client sends `GET /api/v1/custodians?max=N`
- Then the scheduler queries the libp2p DHT/gossip layer for known custodian peers
- And returns a list of up to N `CustodianInfo { peer_id, kem_pubkey_b64 }` — no auth required

#### Scenario: Client retrieves scheduler KEM public key
- Given the scheduler is running
- When a client sends `GET /api/v1/kem_pubkey`
- Then the scheduler returns its X25519 KEM public key in base64 — no auth required

### Requirement: Scheduler selects workers via capability gossip

The scheduler MUST select workers based on capability and resource replies before dispatch.

#### Scenario: Worker selection for a new workload
- Given a workload requires certain capabilities (e.g. `["gpu"]`)
- When the scheduler initiates worker selection
- Then it broadcasts `CapabilityQuery` on the `podmesh-workload` gossipsub topic
- And it collects `CapabilityReply` responses from eligible peers
- And it optionally sends `ResourceQuery` to candidates
- And it selects a worker peer via round-robin or load-based ordering

### Requirement: Scheduler injects sidecar metadata after workload deployment

After deploying a workload, the scheduler MUST write `SidecarMetadata` for the sidecar process.

#### Scenario: Sidecar metadata injection
- Given a worker has deployed a workload container
- When deployment completes
- Then the scheduler writes a `SidecarMetadata` JSON file to
  `/var/run/podmesh/sidecar/metadata.json` inside the workload's filesystem namespace
- And `SidecarMetadata` contains: manifest_id, manifest_b64, owner_public_key_b64, bootstrap_peer

## OBSERVED Trust Gaps

### Observation: Scheduler does not verify custodian peer_id legitimacy

The scheduler routes `WorkloadAssignmentV2` to whatever `custodian_peer_id` values the client
supplies in `WorkloadSubmission.wrapped_shares`. A malicious client could name arbitrary peers.

### Observation: Envelope middleware does not check signer against a trusted list

REST endpoints behind the envelope middleware decrypt and verify Ed25519 signatures, but the
signer pubkey is not validated against any allowlist or node certificate chain. Any peer
with a valid keypair can construct a signed envelope.
