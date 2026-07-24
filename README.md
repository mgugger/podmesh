# Podmesh

Podmesh is a zero-trust, multi-tenant workload mesh. A namespace is currently identified by its
Ed25519 public key. Complete workload specifications are encrypted by `podctl` and sent directly to
the selected execution agent; the scheduler never receives workload plaintext or key material.

## Architecture

```text
podmesh-agent --signed, expiring availability--> podmesh-scheduler
podctl -------candidate selection-------------> podmesh-scheduler
podctl ==encrypted admission and grant========> podmesh-agent
podmesh-agent --Podman + sidecar--------------> workload
podctl ==encrypted status/log/delete==========> podmesh-agent
```

- `podmesh-scheduler` stores only expiring agent advertisements in memory and deterministically
  selects an available candidate. Restarting it does not affect running workloads.
- `podmesh-agent` admits and runs many workloads up to configured count and aggregate resource
  limits. It owns Podman, sidecar injection, local status/log/delete commands, persistent node
  keys, and one encrypted record per workload.
- `podctl` owns the namespace signing key, encrypts the complete execution specification with a
  random DEK, wraps the DEK to the selected agent, and stores the signed deployment receipt locally.
- `podmesh-proxy` and `podmesh-sidecar` remain the workload traffic plane.

## Availability Contract

Automatic recovery is intentionally not implemented yet. A container or agent restart can recover
from the agent's encrypted local record as long as its persistent node keys remain available. Loss
of the only agent and its durable state requires the namespace owner to deploy again. Workloads that
cannot accept this must use multiple replicas once replica handoff is implemented.

## Trust Model

- Agent advertisements are public, signed, bounded, and short-lived.
- Admission requests, deployment grants, receipts, status, logs, and deletion are encrypted between
  `podctl` and the selected agent.
- Owner signatures bind namespace, full 256-bit workload/revision IDs, target node, reservation,
  ciphertext, wrapped DEK, expiry, and nonce.
- Client and agent use the same strict Kubernetes quantity parser. The agent validates post-sidecar
  CPU, memory, and ephemeral storage limits against the signed reservation before execution.
- The selected agent necessarily sees plaintext while executing the workload. Other agents, the
  scheduler, and proxies do not receive the execution specification.
- Proxy traffic confidentiality requires TLS or another end-to-end protocol terminating in the
  workload/sidecar; proxies are not trusted with workload plaintext.

## Build And Run

```bash
cargo build --workspace

./target/debug/podmesh-scheduler --listen 127.0.0.1:3000
./target/debug/podmesh-agent \
  --listen 127.0.0.1:3100 \
  --advertise-url http://127.0.0.1:3100 \
  --scheduler-url http://127.0.0.1:3000 \
  --max-workloads 100 \
  --runtime mock

./target/debug/podctl --api-url http://127.0.0.1:3000 apply -f deploy/demo_deployment.yml
```

Use `--runtime podman` for real execution. The agent expects a working `podman` command and may use
`PODMAN_HOST` to target a mounted Podman socket.

## Test

```bash
cargo test --workspace
```

Podman-dependent tests remain behind the integration test crate's `podman-tests` feature and require
a working Podman CLI, a rootless or rootful Podman socket, and all images from
`deploy/build_containers.sh`. They fail explicitly rather than reporting a skipped test as passing.
The build script creates scratch images for both `linux/amd64` and `linux/arm64` by default.

```bash
cargo test -p podmesh-integration-tests --features podman-tests
```

## Crates

| Crate | Responsibility |
|---|---|
| `podctl` | Namespace keys, encrypted deployment, local receipt catalog |
| `podmesh-scheduler` | Stateless in-memory agent selection |
| `podmesh-agent` | Admission, encrypted persistence, Podman, sidecar injection |
| `podmesh-proxy` | Ingress/egress workload traffic gateway |
| `podmesh-sidecar` | Workload-local traffic endpoint |
| `shared/crypto` | Ed25519, X25519, XChaCha20-Poly1305 |
| `shared/protocol` | Bounded signed/encrypted wire records |
| `shared/p2p` | Proxy/sidecar libp2p transport helpers |