# Podmesh

Podmesh runs multi-tenant workloads across an open mesh of execution agents. A namespace is
identified by its Ed25519 public key. Complete workload specifications are encrypted by `podctl` for
the selected execution agent and relayed through a scheduler; the scheduler carries opaque bytes.

The mesh is deliberately open: anyone with a keypair can ask a scheduler to place a workload, and no
component authenticates *who* is deploying. What is protected is everything after that — reading,
altering, or deleting a workload requires the namespace private key, and a workload cannot reach the
host or another tenant.

See [Trust Model](#trust-model) for what this does and does not protect, including the parts that are
explicitly out of scope for the first release.

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
  agent. A scheduler that does not hold the target attachment finds the one that does with a single
  gossiped location query — never by connecting to peers one at a time — and caches the answer, so
  the cost of an operation does not grow with the size of the mesh. Restarting it does not affect
  running workloads.
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

Every replica registers the same routing key, and the proxy holds one backend per replica and
rotates between them, so ingress is spread across all of them and an unreachable replica is skipped
rather than blackholing the request.

A selection answers as soon as it has collected the number of offers the client asked to choose
between, so placement latency tracks how fast agents answer rather than the query lifetime — which
matters because replicas are placed one at a time.

## Finding Workloads

`podctl` keeps the only index of where it placed replicas, under `~/.podmesh/workloads/`. Because a
lost or overwritten index would otherwise leave workloads running that nothing can address,
`podctl list` asks the mesh instead: the scheduler relays an owner-signed request to every agent
**attached to it**, and anything the local catalog does not know about is flagged as orphaned. Agents
that did not answer are named, so a partial view is not mistaken for a complete one. The request is
not gossiped mesh-wide, so in a mesh of many schedulers a single call sees only that scheduler's
share of the fleet — ask each scheduler and union the answers.

## Bootstrap Without Shared Secrets

Nothing has to be copied between components by hand:

- Schedulers serve a self-signed, self-expiring `EndpointRecord` at `GET /api/v1/endpoint_record`.
- Agents take scheduler HTTP URLs (`PODMESH_AGENT_SCHEDULER_URLS`), fetch and verify those records,
  then attach over Iroh.
- Schedulers take peer HTTP URLs (`PODMESH_SCHEDULER_PEER_URLS`), including their own, and poll
  them in the background. Peers join the gossip allowlist and relay issuer set as they appear, so
  the mesh converges regardless of start order.
- Schedulers then announce themselves on the gossip mesh, so membership grows past the peers listed
  in any one configuration: two schedulers that share only a common third one learn each other.
  An announcement grants membership only — relay-issuer trust stays with pinned peer URLs.
- Proxies self-generate their relay TLS and auth token. A proxy can adopt a peer's token through
  `GET /api/v1/workload_relay_bootstrap`, which matters because a sidecar carries exactly one token.
- `podctl` bootstraps proxy endpoint records, its own tenant's derived relay token, and relay CA
  certificates from `PODMESH_PROXY_URL`. It never receives the mesh secret those tokens derive from;
  proxies share that among themselves over a separate endpoint.

A record is signed by whichever key the responder chose, so a signature alone proves only internal
consistency, never identity. Two places therefore pin identity rather than trusting the response:

- A scheduler binds each peer URL to one endpoint id and signing key — configured up front, or
  remembered from the first observation. A later mismatch is refused, not silently accepted.
- `podctl` checks every capacity offer against the owner's list of trusted agent signing keys before
  producing any ciphertext, because the offer names the key the workload is sealed to.

The relay bootstrap endpoint discloses a live token in cleartext and is therefore opt-in, and at most
one proxy in a deployment serves it.

## Availability Contract

Remote recovery after loss of an agent and its durable keys is not implemented. A container or agent
restart recovers from the agent's encrypted local record as long as its persistent node keys remain
available. Loss of the only agent and its durable state means the workload is gone; run multiple
replicas if that is unacceptable. Single-replica workloads do not recover offline.

## Trust Model

### What is protected

- Capacity queries and offers are public, signed, bounded, and short-lived. They carry no workload
  or tenant data.
- Admission requests, deployment grants, receipts, status, logs, and deletion are encrypted
  end-to-end between `podctl` and the selected agent, using an ephemeral-static X25519 exchange whose
  AEAD key is derived from the shared secret together with both public keys. Low-order recipient keys
  and rewritten blob headers are refused.
- Every signature is bound to its message type by a domain label, so a signature produced for one
  record can never verify as another.
- Owner signatures bind namespace, full 256-bit workload/revision IDs, **target agent**, reservation,
  ciphertext, wrapped DEK, issue time, expiry, and nonce. Every owner-signed message has a bounded
  lifetime, so a captured message stops being usable.
- **The owner chooses which agents may read its workloads.** `podctl` refuses to deploy to an agent
  whose signing key is not in its trusted list. Without that check, whoever answers a selection
  request would choose the recipient — and therefore who can read the plaintext.
- The selected agent necessarily sees plaintext while executing the workload. Other agents, the
  scheduler, and proxies do not receive the execution specification.
- A workload cannot reach the host or another tenant: the agent evaluates every manifest against a
  deny-by-default pod security policy covering privileged execution, capabilities, host namespaces,
  host ports, and volumes of every kind, across `containers` and `initContainers` alike.
- A scheduler admitted only by gossip announcement can use a peer's machine relay for itself, but
  cannot admit anyone else onto it: the announcement binds its signing key to its endpoint, and a
  relay honours such a grant only when the subject is that endpoint — which the transport has already
  authenticated. Unrestricted issuer trust, which can name any subject, still requires pinning.
- A sidecar proves its tenancy rather than asserting it. `podctl` mints an owner-signed workload
  credential at deploy time and seals it into the execution specification; the sidecar presents it
  during the proxy handshake, and the proxy verifies it against the owner key. Because that key is
  also the credential's signing root, naming a tenant is worthless without the tenant's private key.
- A routing key belongs to exactly one tenant. It is derived from the owner key and workload name,
  recomputed by the proxy, and must match the tenancy the connection proved — so claiming a route
  needs the tenant's private key, not merely its public one. Hostnames are first-claim-wins per
  owner and are never reassigned.
- Egress tunnels are authorised by tenant: the connection must have proven an owner, and that owner
  must hold a live grant for this proxy. Destinations are not filtered by address, because app parts
  legitimately live on private networks once components run on different machines.
- Relay credentials are derived per tenant from a mesh secret the workload never receives, so a
  leaked pod cannot relay for another tenant, and `--relay-tenants` bounds whose traffic a proxy
  carries at all.
- Admissions that have not deployed are bounded twice — per namespace, and as a share of agent
  capacity — so an unfinished admission cannot drive an agent's advertised capacity to zero and hold
  it there. Running workloads are deliberately not bounded this way.
- The scheduler client API and the proxy REST API are rate limited per peer address, keyed on the
  connection's peer rather than on a caller-supplied header. Liveness routes stay unthrottled so a
  busy component is not dropped from its orchestrator's pool.
- Proxies authenticate to sidecars with owner-signed, expiring Biscuit grants minted by `podctl`
  and posted to `POST /api/v1/proxy_grant`. A grant binds the tenant owner key, the proxy endpoint
  id, and an expiry. Grants are not used for external ingress.

### What is not protected in this release

These are deliberate scope choices, not oversights:

- **Anyone can deploy.** The scheduler client API is unauthenticated and there is no per-owner
  workload quota. A caller can enumerate agents, occupy placement capacity, and cause mesh-wide
  fan-out. Every path is concurrency-bounded, size-bounded, and rate limited per peer address — but
  none is access controlled. A rate limit bounds how fast anyone may call, not who may.
  One tenant may consume an agent by genuinely deploying to it; the assumption is that capacity
  exists elsewhere. What it may not do is hold capacity without deploying — see below.
- **Tenants share a workload network.** One tenant's pod can reach another's by address. Tenant
  separation is enforced by owner identity at the proxy — relay credentials, route ownership and
  egress are each bound to the owner key — not by network topology, which cannot be a boundary once
  agents, proxies and schedulers run on different machines.
- **Ingress is plain HTTP.** Terminate TLS in front of the proxy if you need it.
- **`POST /api/v1/proxy_grant` is unauthenticated.** A grant authenticates itself, so this cannot
  forge authority for an owner whose key you lack. It is rate limited per peer so it cannot be used
  to cheaply fill the bounded grant store.
- **Proxies are not trusted with workload plaintext.** Traffic confidentiality through a proxy
  requires TLS or another end-to-end protocol terminating in the workload.
- **One installation, one tenant.** `podctl` holds a single namespace identity; multi-tenant
  deployments are supported, a multi-tenant CLI is not.
- **No credential rotation or revocation.** Relay tokens are derived per tenant from a mesh secret,
  so a leaked pod exposes only its own tenant's relay access and `--relay-tenants` can cut one
  tenant off; rotating the mesh secret still re-issues for everyone. Beyond that,
  a proxy grant is valid until it expires.
- **Biscuit attenuation is not implemented.** Grants are used as-is.
- **The agent drives a Podman socket equivalent to host control.** The pod security policy is what
  stands between a tenant manifest and the host.

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

`podctl` will not deploy to an agent the owner has not agreed to trust. List the agents' base64
Ed25519 signing keys in `~/.podmesh/trusted_agents` (or `PODMESH_TRUSTED_AGENTS`), or pass
`--trust-any-agent` to accept whichever agent the mesh offers. `podctl whoami` prints your own
namespace identity.

Use `--runtime podman` for real execution. The agent expects a working `podman` command and uses
`CONTAINER_HOST` to target a mounted Podman socket.

For a realistic local mesh — three schedulers, three agents, three proxies, no hand-created secrets
— see [deploy/README.md](deploy/README.md).

## Test

```bash
cargo test --workspace
```

Podman-dependent tests sit behind the integration test crate's `podman-tests` feature and require a
working Podman CLI, a rootless or rootful Podman socket, and all images from
`deploy/build_containers.sh`. They fail explicitly rather than reporting a skipped test as passing,
and they run in CI. The build script produces scratch images for the host architecture; it refuses a
cross-architecture build rather than producing one silently.

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
| `shared/crypto` | Ed25519 with domain separation, X25519 sealed boxes, XChaCha20-Poly1305 |
| `shared/protocol` | Bounded signed/encrypted wire records |
| `shared/iroh_support` | Common Iroh endpoint and record helpers |