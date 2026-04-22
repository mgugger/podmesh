# Podmesh

A decentralized, zero-trust, multi-tenant compute mesh built on [libp2p](https://libp2p.io/).

Workload specs are **encrypted client-side** before submission. No single node — including the scheduler — ever sees plaintext workload specs or raw decryption keys.

**License**: GPL-3.0-only

---

## Architecture

```
podctl  ──────────────────────────────────────────────────────────────┐
  │  seal spec client-side (Umbral PRE)                               │
  │  encrypt under owner Umbral key                                   │
  │  generate N kfrags, wrap each to a custodian's X25519 KEM pubkey  │
  │                                                                   │
  ▼                                                                   ▼
podmesh-scheduler ◄──── libp2p gossipsub/kad/request-response ──► custodian nodes
  │  routes sealed WorkloadSubmission                                 │
  │  never sees plaintext                                             │  hold kfrags
  ▼                                                                   │
worker nodes ◄──── collect threshold cfrags ────────────────────────┘
  │  reconstruct via Umbral PRE, decrypt spec, deploy
  ▼
container runtime (Podman / mock)
```

### Components

| Crate | Description |
|---|---|
| `podmesh-scheduler` | libp2p node: scheduler, worker, and custodian roles (co-located by default). Exposes a REST API on port 3000. |
| `podctl` | CLI client: seals workload specs, submits to the scheduler, manages deployments. |
| `shared/crypto` | Cryptographic primitives: X25519 KEM, Ed25519 signing, XChaCha20-Poly1305 AEAD, Umbral PRE (v2). |
| `shared/protocol` | Shared message types: `SealedSpec`, `WorkloadSubmission`, `WorkloadDispatch`, `NodeCert`, etc. |
| `shared/p2p` | libp2p swarm helpers. |
| `podmesh-proxy` | Ingress/egress proxy node. |
| `podmesh-sidecar` | In-pod sidecar that connects to the proxy. |

---

## Trust Model

- **Sealing**: happens entirely in `podctl`. The plaintext spec never leaves the client.
- **Umbral PRE**: The spec is encrypted under the owner's Umbral pubkey. The owner generates N key fragments (kfrags), each wrapped to a custodian's X25519 KEM pubkey. Custodians re-encrypt to the target worker's Umbral pubkey — the scheduler sees only capsule bytes and wrapped kfrags, never plaintext.
- **NodeCerts**: Each node self-signs a certificate advertising its capabilities and KEM pubkey. Custodians and workers verify cert chains before accepting assignments.

---

## Quick Start

### Build

```bash
# Build everything (excludes podmesh-sidecar which requires libclang)
cargo build -p podmesh-scheduler -p podctl
```

### Run a local node (co-located scheduler + worker + custodian)

```bash
./target/debug/podmesh-scheduler --mode both --api-port 3000
```

### Submit a workload

```bash
./target/debug/podctl --api-url http://localhost:3000 submit -f deploy/demo_deployment.yml \
  --worker-umbral-pk <hex-or-base64-worker-umbral-pk>
```

### Verify peers / debug

```bash
curl localhost:3000/debug/dht/peers
curl localhost:3000/api/v1/custodians
```

---

## Local Podman Deployment

See [deploy/README.md](deploy/README.md) for running the full stack with Podman.

---

## Testing

Run all tests (excluding Podman-dependent tests):

```bash
cargo test -p podmesh-scheduler -p protocol -p crypto -p podctl
```

Run integration tests (no Podman required):

```bash
cargo test -p podmesh-scheduler --test '*'
```

Run Podman-dependent tests (requires running Podman socket and pre-built images):

```bash
cargo test --package podmesh-scheduler --features podman-tests
```

**Prerequisites for Podman tests:**
- Podman installed and available in `PATH`
- Rootless Podman socket running: `systemctl --user start podman.socket`
- Container images built: `./deploy/build_containers.sh`

---

## Key Files

```
podmesh-scheduler/      main node binary (scheduler + worker + custodian)
podctl/                 CLI client
shared/
  crypto/               cryptographic primitives (KEM, Shamir, Umbral PRE)
  protocol/             shared wire types
  p2p/                  libp2p helpers
deploy/                 Podman deployment manifests and demo YAMLs
```
