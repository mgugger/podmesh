# Podmesh

Project requirements, accepted decisions, and work tracking live in [OpenSpec](openspec/README.md).
This README and the deployment guide explain those specs. The workflow retains adaptive planning,
clarification, approval checkpoints and review using OpenSpec artifacts, without a parallel AI-DLC tree.

## Bounded observability

Schedulers, agents, proxies, and sidecars can expose process-local application metrics with
`--metrics-listen <IP:PORT>` or `PODMESH_METRICS_LISTEN=<IP:PORT>`. Metrics are disabled by default;
an explicitly configured address is an unauthenticated operator endpoint, and bind failure stops
startup rather than silently disabling requested telemetry.

`GET /metrics` returns OpenMetrics text 1.0. `GET /health` reports only the dedicated metrics
listener, not scheduler, agent, proxy, sidecar, storage, relay, or traffic readiness. The four fixed
families use only closed aggregate labels and never include workload manifests, identities, hosts,
URLs, destinations, credentials, grants, tokens, payloads, raw errors, paths, methods, or caller
addresses. Recording and scrape failures cannot change business behavior.

The registry is process-local and not persisted. Counters and histograms reset on restart; gauges
are reconstructed from current runtime state. This MVP defines finite listener and cardinality
bounds, not a production latency, throughput, availability, or recovery SLO. See the deployment
guide for sample ports, network exposure, and direct scrape commands.

Podmesh runs multi-tenant workloads across an open mesh of execution agents. A namespace is
identified by its Ed25519 public key. Complete workload specifications are encrypted by `podctl` for
the selected execution agent and relayed through a scheduler; the scheduler carries opaque bytes.

The mesh is deliberately open: anyone with a keypair can ask a scheduler to place a workload, and no
operator admission policy restricts new owner identities. Owner signatures authorize lifecycle
operations; mesh traffic uses owner-issued credentials. The selected agent and host remain trusted
with execution plaintext, and shared workload networking does not isolate tenants' application ports.

See [Trust Model](#trust-model) for what this does and does not protect, including the parts that are
explicitly out of scope for the first release.

## Glossary

- **Owner / namespace / tenant:** the Ed25519 identity that signs a deployment. These terms refer to
  the same security principal.
- **Deployment:** one owner-scoped workload name and its fixed replica count.
- **Workload:** one independently admitted replica on one agent.
- **Revision:** the content hash of the canonical application manifest.
- **Replica:** one workload identity, indexed within a deployment and pinned to one agent.
- **Scheduler attachment:** an agent's authenticated, ephemeral Iroh connection to a scheduler.
- **Reconciliation:** rebuilding client knowledge by asking authoritative agents; schedulers do not
  become workload databases.

## Architecture

```text
podmesh-agent ==persistent Iroh attachment===> podmesh-scheduler
podmesh-scheduler <==signed Iroh gossip======> podmesh-scheduler
podctl --HTTP: select an agent---------------> podmesh-scheduler
podctl ==HTTP: encrypted admission/grant=====> podmesh-scheduler ==Iroh==> agent
podctl ==HTTP: encrypted update/status/log/delete==> scheduler ==Iroh==> agent
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

## Apply And Update

Applying the same owner and workload name addresses the existing deployment. An unchanged revision
is idempotent. A changed revision is replaced sequentially on the replicas' existing agents using an
owner-signed expected-revision compare-and-swap; updates do not move replicas. The catalog is saved
after each confirmed replica, so a partial failure leaves successful replicas at the new revision
and failed replicas at their last confirmed revision for a later retry.

Changing the replica count of an existing deployment is refused in the MVP. Scaling, surge
placements, self-healing, and relocation are outside the release contract.

Proxy records, proxy grants, relay credentials, workload credentials, and the injected sidecar are
mandatory in production. Re-applying within the renewal window refreshes expiring service-mesh
authority without changing the workload identity, placement, routing key, or application revision.

## Supported Manifest Subset

An MVP manifest contains exactly one `Pod` or `Deployment`, normalized to one runtime pod, plus
optional `Service` and `Ingress` documents. Secrets, persistent storage objects, additional
pod-bearing controllers, unknown kinds, and ephemeral containers are rejected before execution.
The agent injects the sidecar; tenant manifests cannot disable it.

## Finding Workloads

`podctl` keeps its index of where it placed replicas under `~/.podmesh/workloads/`. Because a
lost or overwritten index would otherwise leave workloads running that nothing can address,
`podctl list` asks any scheduler to reconcile through the mesh. The scheduler gossips a bounded,
signed request to admitted peers; each peer queries only its locally attached agents and returns
the agents' owner-sealed answers directly to the requesting scheduler. `podctl` verifies current
receipts and agent identity material and atomically repairs missing or stale catalog placements.
Schedulers retain only bounded, expiring coordination state and never learn workload identities.
Unreachable schedulers and agents are reported explicitly. A missing catalog is never treated as a
new deployment while that view is partial, preventing duplicate stable workload IDs.

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
  proxies share that among themselves over a separate endpoint. Before using any value, `podctl`
  requires an explicit local binding from the proxy URL to its endpoint id and signing key.

A record is signed by whichever key the responder chose, so a signature alone proves only internal
consistency, never identity. Three places therefore pin identity rather than trusting the response:

- A scheduler binds each peer URL to one endpoint id and signing key — configured up front, or
  remembered from the first observation. A later mismatch is refused, not silently accepted.
- `podctl` checks every capacity offer against the owner's list of trusted agent signing keys before
  producing any ciphertext, because the offer names the key the workload is sealed to.
- `podctl cert trust-proxy --proxy-url <url>` explicitly records a proxy's complete URL, endpoint id,
  and signing key before apply or grant may authorize it. A later mismatch is refused until the owner
  runs the same command with `--replace`.

Proxy trust and agent keys share the versioned `~/.podmesh/trusted_agents` registry. Legacy bare
agent-key files are read as before and migrate on the first trust mutation; unique keys are kept but
comments are dropped. Unix permissions are narrowed to `0600` before reading. Removal is idempotent,
concurrent writers are last-successful-writer-wins, and a reported directory-sync failure requires
listing/reloading trust before retry because the complete rename may already be visible. The typed
header deliberately makes older `podctl` binaries fail instead of silently ignoring proxy entries.

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
  manifest plaintext. Selection exposes resource requirements, agent identities, exclusions, and
  candidate counts; workload confidentiality does not imply metadata confidentiality.
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
- The agent checks manifests for prohibited privilege and host-access settings, including host
  namespaces, host ports, and volumes, across `containers` and `initContainers`. These policy checks
  are not a guarantee of host confinement or tenant network isolation.
- A scheduler admitted only by gossip announcement can use a peer's machine relay for itself, but
  cannot admit anyone else onto it: the announcement binds its signing key to its endpoint, and a
  relay honours such a grant only when the subject is that endpoint — which the transport has already
  authenticated. Unrestricted issuer trust, which can name any subject, still requires pinning.
- A sidecar proves its tenancy rather than asserting it. `podctl` mints an owner-signed workload
  credential at deploy time and seals it into the execution specification; the sidecar presents it
  during the proxy handshake, and the proxy verifies it against the owner key. Because that key is
  also the credential's signing root, naming a tenant is insufficient without a valid owner-issued
  credential. The credential is a bearer token; its holder need not possess the owner's private key.
- A routing key belongs to exactly one tenant. It is derived from the owner key and workload name,
  recomputed by the proxy, and must match the tenancy the connection proved — so claiming a route
  needs a valid owner-issued workload credential, not merely the public owner key. Hostnames are
  first-claim-wins per owner and are never reassigned.
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
- Every workload control request and response uses a signed, addressed, fresh, operation-specific
  envelope with bounded replay detection. Duplicate nonces are refused while retained; saturation
  can evict still-valid entries, and process restart clears history. This is not an exactly-once
  guarantee. Replay defaults are 10000 peers, 1000 nonces per peer, 128-byte
  nonces, and 120 seconds. Lower values can be configured with
  `PODMESH_WORKLOAD_REPLAY_MAX_PEERS`, `PODMESH_WORKLOAD_REPLAY_NONCES_PER_PEER`,
  `PODMESH_WORKLOAD_REPLAY_MAX_NONCE_BYTES`, and `PODMESH_WORKLOAD_REPLAY_RETENTION_SECS`.
- HTTP bodies stream through Iroh in 64 KiB chunks up to 16 MiB. WebSocket upgrades are supported as
  opaque bytes after a signed status 101 transition, bounded to 512 MiB per direction, five minutes
  idle, and one hour total. Application signing keys are verified per envelope, not session-pinned.

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
  egress are each bound to the owner key. This does not authorize or filter direct traffic to
  application ports. Network isolation is outside this PoC, not inherently incompatible with
  multi-host deployment.
- **Ingress is plain HTTP.** Terminate TLS in front of the proxy if you need it.
- **`POST /api/v1/proxy_grant` is unauthenticated.** A grant authenticates itself, so this cannot
  forge authority for an owner whose key you lack. It is rate limited per peer so it cannot be used
  without a request budget, but new self-created owner identities can still consume grant capacity.
- **Application traffic is not automatically end-to-end encrypted.** Iroh encrypts the
  sidecar-to-proxy hop; a traffic proxy can read application bytes unless an additional end-to-end
  protocol protects them. External ingress is HTTP. TLS terminated in front of the proxy protects
  that external hop, not application confidentiality from the proxy. Execution specifications
  remain encrypted to the selected agent and are not delivered to proxies.
- **Proxy grants are in memory.** Proxy restart loses them; an owner must repost grants with
  `podctl cert grant-proxy` before affected traffic can resume. An unchanged `podctl apply` can
  return before reposting grants. Manual recovery is accepted for this PoC.
- **One installation, one tenant.** `podctl` holds a single namespace identity; multi-tenant
  deployments are supported, a multi-tenant CLI is not.
- **No immediate credential revocation.** Re-applying renews expiring proxy grants and workload
  credentials, but already issued authority remains valid until expiry. Relay tokens are derived per
  tenant from a mesh secret; rotating that secret still re-issues for everyone.
- **Biscuit attenuation is not implemented.** Grants are used as-is.
- **The agent drives a Podman socket equivalent to host control.** The pod security policy is what
  stands between a tenant manifest and the host.

Statelessness applies to scheduler workload coordination, not to the entire system: agents retain
local workload records and keys, owners retain keys and catalogs, and proxies hold volatile grants.
Decentralization means client-driven placement and multiple scheduler entry points without a central
workload database or leader. It does not provide Byzantine availability, independent-host attestation,
or bootstrap-free discovery. The precise guarantees and unresolved implementation gaps are recorded
in the [PoC scope](docs/poc-scope.md).

## Build And Run

Build with `cargo build --workspace`, then follow the tested
[local mesh walkthrough](deploy/README.md). A useful deployment requires schedulers, agents,
proxies, relay credentials, and the sidecar image; starting only a scheduler and mock agent is not a
workload-plane quick start.

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

## Verification

This is a greenfield PoC. Use normal development checks:

```bash
cargo test --quiet --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

Unit, integration, adversarial and property tests run in the default workspace suite. For real
containers, follow the [Podman walkthrough](deploy/README.md) and enable `podman-tests` after
building current images and starting the Podman socket. CI runs normal checks directly; its manual
`run_podman` input enables real container tests. Keep results and untested assumptions with the
relevant OpenSpec change. The [PoC scope](docs/poc-scope.md) describes what is and is not protected.

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