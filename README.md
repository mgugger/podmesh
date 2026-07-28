# Podmesh

Podmesh is a zero-trust, multi-tenant workload mesh. A namespace is currently identified by its
Ed25519 public key. Complete workload specifications are encrypted by `podctl` for the selected
execution agent and relayed through a scheduler; the scheduler never receives workload plaintext or
key material.

## Architecture

```text
podmesh-agent ==persistent Iroh attachment===> podmesh-scheduler
podmesh-scheduler <==signed Iroh gossip======> podmesh-scheduler
podctl --HTTP: select an agent---------------> podmesh-scheduler
podctl ==HTTP: encrypted admission/grant=====> podmesh-scheduler ==Iroh==> agent
podctl ==HTTP: encrypted status/log/delete===> podmesh-scheduler ==Iroh==> agent
podmesh-agent --Podman + sidecar-------------> workload
```

- `podmesh-scheduler` holds no durable records. It solicits signed, short-lived capacity offers from
  attached agents over Iroh gossip and relays owner-encrypted control payloads to the selected
  agent. Restarting it does not affect running workloads.
- `podmesh-agent` admits and runs many workloads up to configured count and aggregate resource
  limits. It owns Podman, sidecar injection, local status/log/delete commands, persistent node
  keys, and one encrypted record per workload. It serves only `/health` over HTTP; all control
  traffic arrives over Iroh.
- `podctl` is a plain CLI with no Iroh endpoint. It owns the namespace signing key, encrypts the
  complete execution specification with a random DEK, wraps the DEK to the selected agent, talks
  HTTP to any reachable scheduler, and stores the signed deployment receipt locally.
- `podmesh-proxy` and `podmesh-sidecar` remain the workload traffic plane.

## Replica Placement

Replica placement is a client decision. `podctl` reads `spec.replicas` (or the
`podmesh.io/replicas` annotation), pins the manifest it ships to a single pod, and then asks a
scheduler for one agent per replica, passing `?exclude=` so it never lands two replicas on the same
agent. It admits and deploys against each agent itself. The scheduler never learns the replica count
and never fans a deployment out on its own.

## Bootstrap Without Shared Secrets

Nothing has to be copied between components by hand:

- Schedulers serve a self-signed, self-expiring `EndpointRecord` at `GET /api/v1/endpoint_record`.
- Agents take scheduler HTTP URLs (`PODMESH_AGENT_SCHEDULER_URLS`), fetch and verify those records,
  then attach over Iroh.
- Schedulers take peer HTTP URLs (`PODMESH_SCHEDULER_PEER_URLS`), including their own, and poll
  them in the background. Peers join the gossip allowlist and relay issuer set as they appear, so
  the mesh converges regardless of start order.
- Proxies self-generate their relay TLS and auth token. A proxy can adopt a peer's token through
  `GET /api/v1/workload_relay_bootstrap`, which matters because a sidecar carries exactly one token.
- `podctl` bootstraps proxy endpoint records, the relay token, and relay CA certificates from
  `PODMESH_PROXY_URL`.

Records are signed and expiring, so serving them over plain HTTP does not weaken the trust model.
The relay bootstrap endpoint does disclose a live token and is therefore opt-in.

## Availability Contract

Remote recovery after loss of an agent and its durable keys is not implemented. A container or agent
restart recovers from the agent's encrypted local record as long as its persistent node keys remain
available. Loss of the only agent and its durable state means the workload is gone; run multiple
replicas if that is unacceptable. Single-replica workloads do not recover offline.

## Trust Model

- Capacity queries and offers are public, signed, bounded, and short-lived. They carry no workload
  or tenant data.
- Admission requests, deployment grants, receipts, status, logs, and deletion are encrypted
  end-to-end between `podctl` and the selected agent. The scheduler relays opaque bytes it can
  neither read nor forge.
- Owner signatures bind namespace, full 256-bit workload/revision IDs, target node, reservation,
  ciphertext, wrapped DEK, expiry, and nonce.
- Client and agent use the same strict Kubernetes quantity parser. The agent validates post-sidecar
  CPU, memory, and ephemeral storage limits against the signed reservation before execution.
- The selected agent necessarily sees plaintext while executing the workload. Other agents, the
  scheduler, and proxies do not receive the execution specification.
- Proxy traffic confidentiality requires TLS or another end-to-end protocol terminating in the
  workload/sidecar; proxies are not trusted with workload plaintext.
- Proxies authenticate to sidecars with owner-signed, expiring Biscuit grants minted by `podctl`
  and posted to `POST /api/v1/proxy_grant`. A grant binds the tenant owner key, the proxy endpoint
  id, and an expiry, and can later be attenuated and delegated without reissuing. Grants are not
  used for external ingress.

## Build And Run

```bash
cargo build --workspace

./target/debug/podmesh-scheduler --listen 127.0.0.1:3000
./target/debug/podmesh-agent \
  --listen 127.0.0.1:3100 \
  --max-workloads 100 \
  --runtime mock

./target/debug/podctl --api-url http://127.0.0.1:3000 apply -f deploy/demo_deployment.yml
```

Use `--runtime podman` for real execution. The agent expects a working `podman` command and may use
`PODMAN_HOST` to target a mounted Podman socket.

For a realistic local mesh — three schedulers, three agents, three proxies, no hand-created secrets
— see [deploy/README.md](deploy/README.md).

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
| `podmesh-scheduler` | Stateless Iroh placement and owner-payload relay to agents |
| `podmesh-agent` | Admission, encrypted persistence, Podman, sidecar injection |
| `podmesh-proxy` | Ingress/egress workload traffic gateway |
| `podmesh-sidecar` | Workload-local traffic endpoint |
| `shared/crypto` | Ed25519, X25519, XChaCha20-Poly1305 |
| `shared/protocol` | Bounded signed/encrypted wire records |
| `shared/iroh_support` | Common Iroh endpoint and record helpers |