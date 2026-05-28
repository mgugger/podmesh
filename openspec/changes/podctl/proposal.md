# podctl: Client CLI for Workload Submission and Management

## Purpose

Document the behavior of `podctl` as it exists in the codebase.
This is a descriptive spec — it records observed implementation, not intended design.

## Component Location

`podctl/src/`

## Role

`podctl` is the operator-facing CLI. It:
1. Seals workload specifications client-side before any data leaves the machine
2. Submits sealed workloads to the mesh via the scheduler REST API
3. Manages workloads (delete, status, logs)
4. Converts Kubernetes-style manifests to podmesh format

## Commands

Source: `src/main.rs`

| Command | Description |
|---|---|
| `apply -f <file>` | Seal and submit a workload manifest |
| `delete -f <file>` | Delete a deployed workload |
| `get pods` | List running workloads |
| `get pod <id>` | Get workload detail |
| `logs <id>` | Get workload logs |
| `convert -f <file>` | Convert Kubernetes manifest to podmesh format |
| `cert <subcommand>` | NodeCert tooling (`issue`, `show`, `verify`, `grant-proxy`) |

## Key Flags

- `--api-url` — scheduler REST API base URL (default `http://127.0.0.1:3000`)
- `--shares` — number of Shamir shares (N)
- `--threshold` — Shamir threshold (K)
- `--capability` (repeatable) — required worker capabilities
- `--output` — output format for `get` (`table`/`json`)

## Observed Behavior

### `apply` — Workload Sealing and Submission

Source: `src/lib.rs:apply_file()`, `src/seal.rs:seal_workload()`

Full sequence:

1. Read manifest file (YAML or JSON).
2. Parse YAML → JSON string (`spec_json`).
3. Load or generate Ed25519 signing keypair from `~/.podmesh/` (persistent).
4. `GET {server}/api/v1/custodians?max=N` → `Vec<CustodianInfo { peer_id, kem_pubkey_b64 }>`.
   Aborts if fewer than N custodians are available.
5. Call `seal_workload(spec_json, custodians, owner_pk, owner_sk, n, k, capabilities)`:

   a. Validate `n >= k >= 1` and `custodians.len() >= n`.

   b. Call `crypto::seal_shamir(spec_json_bytes, custodians_with_kem_pubs, k)`:
      - Generate random 32-byte DEK.
      - XChaCha20-Poly1305 encrypt `spec_json_bytes` with DEK → `(ciphertext, 24-byte nonce)`.
      - Split DEK into N Shamir shares over GF(256).
      - ECIES-wrap each share to the corresponding custodian's X25519 KEM public key.

   c. Compute `manifest_id = blake3(spec_json_bytes)[..8].to_lowercase_hex()` (16-char hex).

   d. Build `SealedSpec { manifest_id, owner_pubkey, ciphertext, nonce, kfrag_count=N,
      kfrag_threshold=K, sealed_at_secs, submission_version=1, replica_count }`.
      (`replica_count` is parsed from the manifest, defaulting to 1)

   e. `submission_sig = Ed25519.sign(owner_sk, postcard(sealed_spec))`.

   f. Build `Vec<SubmittedShare>` from `ShamirSealOutput.wrapped_shares` (one per custodian).

   g. Return `WorkloadSubmission { sealed_spec, required_capabilities, submission_sig,
      wrapped_shares, replica_count }`.

6. `POST {server}/api/v1/workloads/submit` with `WorkloadSubmission` JSON body.
7. Print response (`manifest_id`, assigned custodians).

### `delete` — Workload Deletion

Source: `src/lib.rs:delete_file()`

1. Read manifest file → `spec_json`.
2. Compute `manifest_id = blake3(spec_json_bytes)[..8].hex()`.
3. `POST {server}/tasks/{manifest_id}/providers` → discover peer_ids holding the manifest.
4. For each provider peer:
   a. Load owner Ed25519 keypair.
   b. Build `DeleteRequest { manifest_id }`.
   c. Encrypt as an `Envelope` (ECIES to the peer's KEM pubkey + Ed25519 signed).
   d. `POST {server}/delete_direct/{peer_id}` with the encrypted envelope body.

### `get` / `logs` — Status Queries

Source: `src/lib.rs:get_pods()`, `get_pod()`, `get_logs()`

Plain HTTP GET to:
- `GET {server}/runtime/workloads` — list workloads
- `GET {server}/runtime/workloads/{id}` — workload detail
- `GET {server}/runtime/workloads/{id}/logs` — logs

No authentication on these endpoints.

### `convert` — Kubernetes Manifest Conversion

Source: `src/convert.rs`

Reads a Kubernetes-style multi-document YAML (`Deployment`, `Service`, `Ingress`, `ConfigMap`)
and converts it to a podmesh manifest format. Outputs JSON or YAML.

## Key Material

- Ed25519 signing keypair: `~/.podmesh/pubkey.bin` + `~/.podmesh/privkey.bin`
- X25519 KEM keypair: `~/.podmesh/kem_pub.bin` + `~/.podmesh/kem_priv.bin`
- Keys are generated on first use and persisted with mode `0o600`.
- No top-level `--ephemeral` CLI flag is currently exposed.

## Trust Assumptions (as implemented)

- `podctl` trusts the scheduler's `GET /api/v1/custodians` response without any verification that
  returned custodians are legitimate mesh nodes. A compromised scheduler can return attacker
  KEM pubkeys, causing DEK shares to be ECIES-wrapped to attacker keys.
- The custodian list source of truth is the scheduler — there is no independent custodian
  directory that podctl can verify.
- `GET /runtime/workloads` and related endpoints are unauthenticated — anyone with network
  access to the scheduler can query running workload metadata.
- The `manifest_id` is a truncated blake3 hash (8 bytes, 16 hex chars). Collision probability
  across large manifests is non-negligible in high-volume deployments.

## Data Flow

```
Operator workstation
  │ podctl apply -f workload.yaml --shares 5 --threshold 3
  ▼
podctl
  │ read workload.yaml → spec_json
  │ load/generate Ed25519 + X25519 keypairs from ~/.podmesh/
  │ GET {server}/api/v1/custodians?max=5
  │  ← Vec<CustodianInfo { peer_id, kem_pubkey_b64 }>
  │ seal_workload: generate DEK, encrypt spec, Shamir split, ECIES wrap per custodian
  │ POST {server}/api/v1/workloads/submit [WorkloadSubmission JSON]
  │  ← WorkloadSubmissionResponse { manifest_id, custodian_peers }
  ▼
Operator receives manifest_id (e.g. "a3f9c1e20b4d7f88")
```

## Non-Goals of This Spec

- Does not describe mesh-side processing (see podmesh-scheduler spec)
- Does not describe crypto primitives in detail (implemented in shared/crypto)
- Does not describe policy enforcement (handled server-side)
