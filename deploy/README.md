# Try Podmesh Locally

This is the podmesh equivalent of "start minikube, then run kubectl against it". You bring up a
full mesh — 3 schedulers, 3 agents, 3 proxies — on your own machine with `podman kube play`, then
drive it with `podctl`.

Nothing has to be created by hand. Every Iroh identity, relay TLS keypair, and relay auth token is
generated on first start and persisted in the `podmesh-state` volume. There are no Kubernetes
Secrets in the manifests.

## Application metrics

The sample manifests explicitly enable bounded application metrics inside the `podmesh` network:

| Runtime | Application metrics | Existing relay metrics |
|---|---|---|
| schedulers 1-3 | `podmesh-control:9200-9202` | `podmesh-control:9090-9092` |
| agents 1-3 | `podmesh-agents:9210-9212` | none |
| proxies 1-3 | `podmesh-proxies:9220-9222` | `podmesh-proxies:9100-9102` |
| each workload sidecar | workload pod address on 9230 | none |

Application metrics use `PODMESH_METRICS_LISTEN`. Agents receive
`PODMESH_AGENT_SIDECAR_METRICS_LISTEN=0.0.0.0:9230` and inject the universal setting into sidecars
outside their signed metadata. These unauthenticated ports have `containerPort` declarations but no
`hostPort`; any peer that can reach the Podman network can scrape them. That placement is not
authentication or tenant isolation.

The runtime images are scratch-based, so use a disposable network client rather than `podman exec`:

```bash
podman run --rm --network podmesh docker.io/curlimages/curl:latest \
  --fail --silent --show-error --max-time 5 \
  http://podmesh-control:9200/metrics
```

Repeat for scheduler 9200-9202, agent 9210-9212, and proxy 9220-9222. `GET /metrics` ends with
`# EOF`. The metrics listener's `GET /health` returns `ok` but is not business readiness; retain the
capacity, agent, ingress, and traffic checks below. No collector, dashboard, alert, trace service,
metric persistence, or quantitative SLO is included.

Every command and every output below was run against this repository.

---

## Step 1 — Prerequisites

You need Podman with its API socket running, a network for the mesh, the four container images, and
the `podctl` binary.

```bash
podman --version                      # 6.0.1 was used here
systemctl --user start podman.socket  # rootless API socket the agents drive
podman network create podmesh
./deploy/build_containers.sh          # podmesh/{scheduler,agent,proxy,sidecar}:latest
cargo build -p podctl
```

Check the socket and the images:

```bash
ls -l /run/user/$(id -u)/podman/podman.sock
podman images --format '{{.Repository}}:{{.Tag}}' | grep podmesh
```

```
localhost/podmesh/scheduler:latest
localhost/podmesh/agent:latest
localhost/podmesh/proxy:latest
localhost/podmesh/sidecar:latest
```

The agent injects `podmesh/sidecar:latest` into every workload pod, so that image must exist locally
before you deploy anything.

## Step 2 — Start the mesh

This is the "minikube start" of podmesh.

```bash
podman kube play deploy/podmesh_rootless.yml --network podmesh
```

For a rootful Podman (agents drive `/run/podman/podman.sock`):

```bash
sudo podman kube play deploy/podmesh_rootful.yml --network podmesh
```

If your rootless socket is not under UID 1000, adjust the `podman-socket` hostPath in
`deploy/podmesh_rootless.yml`.

Three pods come up:

```bash
podman pod ps
```

```
POD ID        NAME             STATUS      CREATED        INFRA ID      # OF CONTAINERS
eef308c7e537  podmesh-proxies  Running     2 minutes ago  7da5e644908c  4
5fcecc6f6ee7  podmesh-agents   Running     2 minutes ago  cb98b60ec6d3  4
485c57d3d3be  podmesh-control  Running     2 minutes ago  08dc5b6a0d36  4
```

On a cold volume the schedulers restart once or twice: each one trusts all three relay
certificates, and the first to start finds that its peers have not written theirs yet, so it exits
and `restartPolicy: Always` brings it back. This settles by itself, usually within 15 seconds. The
manifests do not depend on start order.

## Step 3 — Check the mesh is healthy

This is the "kubectl get nodes" of podmesh.

Every scheduler publishes a signed, self-expiring `EndpointRecord`:

```bash
for p in 3000 3001 3002; do curl -s http://127.0.0.1:$p/api/v1/endpoint_record | head -c 60; echo; done
```

Every agent serves a health probe:

```bash
for p in 3100 3101 3102; do curl -s http://127.0.0.1:$p/health; echo; done
```

```
ok
ok
ok
```

The real readiness check is capacity selection. It gossips a signed, short-lived `CapacityQuery`
across the mesh, waits out the offer window, and returns one signed `CapacityOffer`:

```bash
for p in 3000 3001 3002; do
  printf "select@%s " $p
  curl -s --max-time 20 -o /dev/null -w "http=%{http_code} time=%{time_total}\n" \
    "http://127.0.0.1:$p/api/v1/agents/select?cpu_milli=100&memory_bytes=134217728&storage_bytes=1073741824&capabilities=multi-workload"
done
```

```
select@3000 http=200 time=5.006198
select@3001 http=200 time=5.043524
select@3002 http=200 time=5.008195
```

`http=200` from all three means the mesh is up, including `:3000`, which has no agents attached at
all — its offers come from agents attached to its peers. The ~5 s is the offer collection window,
not latency. `http=503 no agent capacity available` means no agent is attached yet — give it a few
more seconds, then read the troubleshooting section.

### What is listening

| Component | HTTP API | Iroh bind | Relay (http/https/qad/metrics) | Attached agents |
|---|---|---|---|---|
| scheduler-1 | 3000 | 5001 | 7070 / 7440 / 7840 / 9090 | none |
| scheduler-2 | 3001 | 5002 | 7071 / 7441 / 7841 / 9091 | agent-1, agent-2 |
| scheduler-3 | 3002 | 5003 | 7072 / 7442 / 7842 / 9092 | agent-3 |
| agent-1 | 3100 (`/health` only) | 5011 | — | — |
| agent-2 | 3101 (`/health` only) | 5012 | — | — |
| agent-3 | 3102 (`/health` only) | 5013 | — | — |
| proxy-1 | 3010 (ingress on 8080) | 4001 | 7080 / 7450 / 7850 / 9100 | — |
| proxy-2 | 3011 | 4002 | 7081 / 7451 / 7851 / 9101 | — |
| proxy-3 | 3012 | 4003 | 7082 / 7452 / 7852 / 9102 | — |

### How the mesh finds itself

* **Schedulers** publish their `EndpointRecord` at `GET /api/v1/endpoint_record`. Each one lists all
  three peer URLs in `PODMESH_SCHEDULER_PEER_URLS`, including its own, and polls them in the
  background. Peers are admitted to the gossip allowlist and the relay issuer set as they appear,
  and their addresses are fed into the endpoint's address book so gossip can dial them.
* **Agents** get `PODMESH_AGENT_SCHEDULER_URLS`, fetch those same records over HTTP, and attach to
  exactly one scheduler over Iroh. The sample topology is deliberately lopsided: agent-1 and agent-2
  attach to scheduler-2, agent-3 attaches to scheduler-3, and **scheduler-1 holds no agents at all**.
  That is the case a real multi-region mesh always has, and it is the one that would otherwise go
  untested.
* **Control traffic crosses schedulers.** Placement already worked from any scheduler, because a
  capacity query is gossiped and agents answer the querying scheduler directly. Lifecycle traffic
  now does too: a scheduler that does not hold the target attachment probes its peers over
  `/podmesh/agent-control-relay/1`, then hands the owner-encrypted payload to the peer that does.
  The hop is taken at most once and the bytes are never touched, so the relaying scheduler learns
  nothing the first one did not already know.
* **Proxies** each run their own relay with their own self-signed TLS. Relay credentials are derived
  per tenant from a mesh secret, so every proxy needs the same secret to validate them: proxy-2 and
  proxy-3 adopt proxy-1's through `PODMESH_WORKLOAD_RELAY_BOOTSTRAP_URL` instead of minting their
  own. Only proxy-1 serves it, over `GET /api/v1/workload_relay_mesh_secret`, and only on the pod
  network — that endpoint hands a mesh-wide credential in cleartext to anyone who can reach it.
  `podctl` uses the separate `GET /api/v1/workload_relay_bootstrap?owner=<key>`, which returns only
  the asking tenant's derived token and never the secret. Set `--relay-tenants` to bound which
  tenants a proxy will relay for; by default it relays for any of them, because relaying is
  transport and what a workload may register or tunnel is decided by its owner-signed credential.

These records are self-signed — the key that validates one is inside it — so a signature proves the
record is internally consistent, not who sent it. Identity is pinned separately: a scheduler binds
each peer URL to one endpoint id and signing key on first sight (`peer_pins.json` in its key
directory) and refuses a later mismatch. Configure `PODMESH_SCHEDULER_PEER_PINS` to remove even that
first-observation window.

### What this local stack does not protect

Worth knowing before you point anything real at it:

* **Anyone can deploy.** The scheduler client API on `:3000`–`:3002` is unauthenticated. It is rate
  limited per peer address (`--client-rate-limit-per-minute`, default 1200), which bounds abuse but
  does not decide who may call.
* **Ingress is plain HTTP** on `:8080`. There is no TLS termination. Iroh encrypts the
  sidecar/proxy hop, but proxies can read application bytes unless an additional end-to-end
  protocol protects them. TLS terminated in front of the proxy protects only that external hop.
* **`POST /api/v1/proxy_grant` is unauthenticated.** A grant authenticates itself, so nobody can
  forge authority for your owner key. It is rate limited per peer
  (`--rest-rate-limit-per-minute`, default 600) so filling the grant store is not free.
* **Agents drive the Podman socket**, which is equivalent to host control in the rootful manifest.
  The pod security policy is what stands between a tenant manifest and your host: volumes of every
  kind, host namespaces, host ports, tenant-added capabilities and explicit privileged settings are
  refused. Manifest checks do not certify host confinement; the injected sidecar may add `NET_ADMIN`.
* **All nine components share one state volume**, so they are not isolated from each other.
* **Tenants share one workload network**, so one tenant's pod can reach another's by address. This
  is by design: mesh operations are authorized by owner identity at the proxy, while direct traffic
  to application ports is not isolated. Network isolation is outside this PoC, not incompatible
  with multi-host deployment.
* **Egress tunnels are not filtered by destination address.** A workload whose owner granted a
  proxy can tunnel anywhere that proxy can reach, including its own loopback services and cloud
  instance metadata. Authorisation is by tenant, not by address, so run proxies where that reach is
  acceptable.
* **Workload control is fully enveloped.** Proxy and sidecar sign every handshake, registration,
  discovery, ingress, egress setup, and announcement request/response. HTTP bodies stream in 64 KiB
  chunks up to 16 MiB; WebSocket streams allow 512 MiB per direction, five minutes idle, and one
  hour total. Upgrade proxy and sidecar together; mixed workload protocol versions are unsupported.
* **Replay limits are configurable.** Use `PODMESH_WORKLOAD_REPLAY_MAX_PEERS`,
  `PODMESH_WORKLOAD_REPLAY_NONCES_PER_PEER`, `PODMESH_WORKLOAD_REPLAY_MAX_NONCE_BYTES`, and
  `PODMESH_WORKLOAD_REPLAY_RETENTION_SECS`; defaults are `10000`, `1000`, `128`, and `120`.
  Duplicates are refused while retained; eviction or process restart can permit replay within the
  freshness window. Signatures and authorization remain mandatory.
* **Proxy grants are in memory.** After proxy restart, repost grants with `podctl cert grant-proxy`.
  An unchanged `podctl apply` can return without reposting them. Manual recovery is accepted.
* **Stateless and decentralized describe scheduler coordination.** Agents retain local workload
  records and keys; owners retain keys and catalogs. Multiple scheduler entry points require no
  central workload database or leader, but configured bootstrap and trusted execution hosts remain
  necessary. Separate agent identities do not prove separate physical hosts.

## Step 4 — Point `podctl` at the mesh

This is the "kubectl config" of podmesh. `podctl` is a plain CLI with no Iroh endpoint of its own;
it speaks HTTP to any scheduler and bootstraps its proxy endpoints, its tenant's relay token, and relay CA
certificates from the proxy REST APIs.

```bash
export PODMESH_API=http://127.0.0.1:3000
export PODMESH_PROXY_URL=http://127.0.0.1:3010,http://127.0.0.1:3011,http://127.0.0.1:3012
```

Any of `:3000`, `:3001`, `:3002` works. Schedulers are stateless and hold no durable agent records,
and one that does not hold an agent's attachment relays through the peer that does — so pointing
`podctl` at `:3000`, which has no agents attached, exercises exactly that path.

Owner keys live in `~/.podmesh/` (or `PODMESH_KEY_DIR`) and are created on first use. They are your
tenant identity: they sign the workload specification, mint the Biscuit grants the proxies present to
sidecars, and are the only keys that can later fetch status, read logs, or delete the workload.
`podctl whoami` prints the public half. Losing them means losing every deployment made under them.

`podctl` also keeps a per-deployment catalog under `~/.podmesh/workloads/`, written after each
replica is confirmed. **It is the only handle on a running deployment** — the mesh keeps no
owner-side index — so back it up if the deployments matter.

### Tell `podctl` which agents it may trust

A capacity offer is self-signed and names the key your workload is sealed to, so whoever answers a
selection request chooses who can read it. `podctl` refuses to deploy until you say which agents are
acceptable:

```bash
# Read each agent's signing key from its logs, or from a mesh you control.
podman logs podmesh-agents-agent-1 2>&1 | grep 'signing key'
```

For this local trial, where the agents are yours, opt out explicitly instead:

```bash
export PODMESH_TRUST_ANY_AGENT=1   # or pass --trust-any-agent on each command
```

In a real mesh, list the trusted agents' base64 Ed25519 signing keys, one per line, in
`~/.podmesh/trusted_agents`.

### Tell `podctl` which proxies it may trust

Each proxy endpoint record is self-signed, so its signature proves internal consistency but does not
prove that the party answering a plain HTTP URL is the proxy you intended. Explicitly record each
configured proxy before apply:

```bash
./target/debug/podctl cert trust-proxy --proxy-url http://127.0.0.1:3010
./target/debug/podctl cert trust-proxy --proxy-url http://127.0.0.1:3011
./target/debug/podctl cert trust-proxy --proxy-url http://127.0.0.1:3012
./target/debug/podctl cert list-proxies
```

The trust command is intentionally non-interactive: invoking it is the decision, and it prints the
full canonical URL, endpoint id, and signing key immediately before committing. If a proxy is rebuilt
with a new identity, inspect it and repeat the command with `--replace`. Ordinary apply never creates
or changes trust.

Proxy bindings and agent keys share the versioned `~/.podmesh/trusted_agents` file. The first
mutation migrates a legacy bare-key file and keeps unique agent keys but drops comments. Unix
permissions are narrowed to `0600` before reading. `remove-proxy` is idempotent. Concurrent writers
are not coordinated, so the last complete atomic write wins. If a trust command reports a
persistence error after rename, run `list-proxies` before retrying because the new complete binding
may already be visible. Older `podctl` binaries refuse the typed header rather than ignoring proxy
trust.

If you prefer to wire things explicitly, unset `PODMESH_PROXY_URL` and supply
`PODMESH_WORKLOAD_RELAY_AUTH_TOKEN` — this tenant's derived token, not the mesh secret — plus
`PODMESH_WORKLOAD_RELAY_CA_CERTS`, with the proxy
`EndpointRecord`s in `PODMESH_PROXY_ENDPOINTS` or in the owner-controlled manifest annotation
`podmesh.io/proxy-endpoints`. `PODMESH_PROXY_URL` wins whenever it is set, because it also tells
`podctl` which proxies to grant.

## Step 5 — Deploy a workload

This is the "kubectl apply" of podmesh.

```bash
./target/debug/podctl --trust-any-agent apply -f deploy/demo_deployment.yml
```

```
Applied workload 477185c50bd648ece5c827c1c2255f0e271f55d195c9112965d3c333b9f1e6f4
```

Behind that single line, `podctl` mints a short-lived owner-signed Biscuit grant for each proxy
(`POST /api/v1/proxy_grant`), asks a scheduler for one agent per replica, then admits and deploys
against each agent — all as encrypted, owner-signed payloads that the scheduler relays but cannot
read.

Re-applying the unchanged file is idempotent. Changing the image or configuration updates replicas
sequentially on their existing agents and records each successful revision immediately. Changing
`spec.replicas` for an existing deployment is refused; scaling is outside the MVP.

The agent injects a sidecar and hands the pod to Podman:

```bash
podman pod ps --format '{{.Name}}' | grep -v 'podmesh-control\|podmesh-agents\|podmesh-proxies'
```

```
podmesh-853b7f7d0b23ba67315293cdcce6212aac49f0871f3d9ac9608-pod
```

## Step 6 — Inspect it

```bash
./target/debug/podctl get pods
```

```json
[
  {
    "deployment_id": "477185c50bd648ece5c827c1c2255f0e271f55d195c9112965d3c333b9f1e6f4",
    "workload_name": "my-nginx",
    "replicas": [
      {
        "replica_index": 0,
        "agent_endpoint_id": "79b5c745781f1c1ec19325e209a77090dae0707bb94d6300818f912cd0b1bf43",
        "receipt": { "...": "owner-verifiable admission receipt" }
      }
    ]
  }
]
```

Logs are fetched per replica, container by container, through the scheduler relay. A deployment can
be addressed by name or by its 64-hex deployment ID:

```bash
./target/debug/podctl logs my-nginx -n 3
```

```json
[
  {
    "replica_index": 0,
    "agent_endpoint_id": "95bcd79db5a8e6d3089098b8a191620f45f4975d298f93bfcd637300dc39c9a2",
    "workload_id": "298aebe7abe91415e8f6818027b173e72bc7dc06844e4969d80c2938afeac62f",
    "payload": "==> ...-pod-my-nginx <==\n2026/07/28 13:34:09 [notice] 1#1: start worker process 33\n..."
  }
]
```

The demo manifest declares an Ingress for `demo-nginx.mesh.local`, so the workload is reachable
through proxy-1's ingress port:

```bash
curl -H 'Host: demo-nginx.mesh.local' http://127.0.0.1:8080/
```

```html
<html>
  <head><title>Podmesh NGINX</title></head>
  <body>
    <h1>Welcome to Podmesh</h1>
  </body>
</html>
```

That request went from curl to proxy-1 over HTTP, from proxy-1 to the injected sidecar over Iroh
QUIC, and from the sidecar to nginx on localhost inside the pod.

## Step 7 — Spread replicas across agents

Replica placement is a client decision; the scheduler never learns the replica count. Set
`spec.replicas` to 3 (or use the `podmesh.io/replicas` annotation):

Delete the single-replica deployment first, or use a different workload name. Re-applying the same
deployment with a different replica count is rejected rather than silently changing placement.

```bash
./target/debug/podctl delete -f deploy/demo_deployment.yml

sed 's/replicas: 1/replicas: 3/' deploy/demo_deployment.yml > /tmp/demo3.yml
./target/debug/podctl --trust-any-agent apply -f /tmp/demo3.yml
```

`podctl` asks for one agent per replica, passing `?exclude=` with the agents it already picked, so
each replica lands somewhere else:

```bash
./target/debug/podctl -o json get pods | grep -E 'agent_endpoint_id|replica_index'
```

Ingress is spread across all three: the proxy holds one backend per replica and rotates between
them, skipping any replica it cannot reach.

To see what the mesh itself is running — including anything whose catalog entry was lost — ask the
agents rather than the local catalog:

```bash
./target/debug/podctl list
```

The scheduler you contact does not need to hold any agent attachments. It gossips the reconciliation
request across the scheduler mesh, and `podctl` rebuilds verified catalog placements from the
owner-sealed agent answers. If any scheduler or agent is unreachable, the output says the view is
partial and `podctl apply` refuses to create a supposedly new deployment from that incomplete view.

```
NAME        REPLICA  STATE     AGENT             ORPHANED
my-nginx    0/3      deployed  95bcd79db5a8e6d3  no
my-nginx    1/3      deployed  b33d1a6242ff906e  no
my-nginx    2/3      deployed  dfd8eaeba764f461  no
```

```
"agent_endpoint_id": "95bcd79db5a8e6d3089098b8a191620f45f4975d298f93bfcd637300dc39c9a2",
"replica_index": 0
"agent_endpoint_id": "b33d1a6242ff906e3ca9b1fbea32239dbae88dc3665071a9a64c09c633631e89",
"replica_index": 1
"agent_endpoint_id": "dfd8eaeba764f461d47df123e770a1403f654274b94d3f9e7bbcd44977f97af7",
"replica_index": 2
```

Three distinct agents, three independent pods:

```bash
podman pod ps --format '{{.Name}}' | grep -v 'podmesh-control\|podmesh-agents\|podmesh-proxies'
```

```
podmesh-40f1814c3524a531a6709103dfd7a5b748e72212ff13ffb4547-pod
podmesh-a9b2e006c54ff89ee8d81573649fb83b34f75356b4e9f19af7a-pod
podmesh-853b7f7d0b23ba67315293cdcce6212aac49f0871f3d9ac9608-pod
```

## Step 8 — Delete the workload

Deletion is an owner-signed command sent to every replica. Every replica is attempted, and the
catalog entry is dropped only once all agents confirm — a replica that could not be deleted stays in
the catalog so the command can be retried. `--force` drops the entry anyway.

```bash
./target/debug/podctl delete -f /tmp/demo3.yml
# or, without the manifest, by deployment id or workload name:
./target/debug/podctl delete my-nginx
```

```
Deleted 477185c50bd648ece5c827c1c2255f0e271f55d195c9112965d3c333b9f1e6f4
```

```bash
podman pod ps --format '{{.Name}}' | grep -v 'podmesh-control\|podmesh-agents\|podmesh-proxies'
./target/debug/podctl get pods
```

```
[]
```

## Step 9 — Tear down the mesh

```bash
podman kube down deploy/podmesh_rootless.yml
podman volume rm podmesh-state    # also discards every identity and all agent state
```

Keeping `podmesh-state` preserves each component's identity across restarts, and agents reconcile
their stored workloads on start. Never run the same key directory from two live processes.

---

## Troubleshooting

**`/api/v1/agents/select` returns 503 `no agent capacity available`.**
No agent is attached. Check the agent log for the attachment handshake:

```bash
podman logs podmesh-agents-agent-1 2>&1 | grep -i 'attach\|bootstrap'
```

```
bootstrapped scheduler endpoint record from http://podmesh-control:3001
agent attached to scheduler acbdd0e706
```

`scheduler attachment ... failed: scheduler issued a grant for an unconfigured relay` means the
relay URL a scheduler advertises is not byte-for-byte in that agent's
`PODMESH_AGENT_MACHINE_RELAY_URLS`. The comparison is an exact string match, so `localhost` and
`podmesh-control` are different relays.

**A lifecycle call returns 404 `agent is not attached to any scheduler in the mesh`.**
The scheduler you addressed does not hold the attachment and no peer claimed it either. Either the
agent is down, or the schedulers have not finished admitting each other yet — the peer probe only
reaches schedulers already in the member allowlist:

```bash
podman logs podmesh-control-scheduler-1 2>&1 | grep 'admitted scheduler'
```

A 502 instead means a peer does hold the attachment but could not reach the agent.

**The logs are drowning in Iroh transport spam.** The manifests already set
`RUST_LOG=info,iroh=warn,iroh_relay=warn,iroh_quinn=warn`. To read a component's own lines only:

```bash
podman logs podmesh-control-scheduler-1 2>&1 \
  | grep -v 'iroh::socket\|tracing::span\|net_report\|remote_map'
```

**A scheduler exits with `inspect CA certificate file ... No such file or directory`.** Expected on
a cold volume: it started before its peers wrote their relay certificates. `restartPolicy: Always`
retries until all three exist.

**`The relay denied our authentication (invalid machine relay grant)`.** Also expected during
convergence: schedulers only trust each other as relay grant issuers after HTTP discovery has
admitted them. It stops once every scheduler logs `admitted scheduler <id>` for the other two.

**The images are `scratch`-based**, so `podman exec <container> sh` cannot work. Diagnose with
`podman logs` only.

**Per-component log commands:**

```bash
podman logs -f podmesh-control-scheduler-1
podman logs -f podmesh-agents-agent-1
podman logs -f podmesh-proxies-proxy-1
```

---

## Utility: inspect a proxy identity

```bash
podman run --rm -v podmesh-state:/var/lib/podmesh \
  podmesh/proxy:latest --init-identity --key-dir /var/lib/podmesh/proxy-1/keys
```

## Automated validation

Everything in this document is also asserted by tests.

```bash
cargo test --workspace
CONTAINER_HOST=unix:///run/user/$(id -u)/podman/podman.sock \
  cargo test -p podmesh-integration-tests --features podman-tests
```

The gated suite requires Podman, its API socket, and current local images. It removes the
`podmesh-state` volume so every run starts from cold identities, plays the same manifest you used
in step 2, and then repeats the tutorial:

* `complete_rootless_stack` waits for the scheduler API, waits for `GET /api/v1/agents/select` to
  return an offer, applies the sample manifest with `PODMESH_PROXY_URL` pointed at the three proxy
  REST APIs, verifies status, logs, ingress, fixed-placement update, catalog reconstruction through
  the scheduler with no attached agents, and deletion. It then deploys three replicas on distinct
  agents, updates them sequentially while continuously probing shared ingress, and verifies complete
  deletion.
* `podman_transparent_egress_test` deploys a workload with egress enabled and asserts that outbound
  traffic is intercepted by the sidecar and tunnelled through a proxy.

Both derive the pod name from the per-replica workload id (`protocol::workload_id`), which is what
the agent names its pods after; `podctl` prints the deployment id instead.

## Local Image Tags and Verification

Local image names default to `latest`. A custom tag is optional; the builder prints each local
image identity so operators can see what was built:

```bash
PODMESH_IMAGE_TAG=poc-local-1 ./deploy/build_containers.sh
```

The provided manifests and tests use `latest`; build without an override or retag all four images
before running them. With current images and a working socket, run:

```bash
cargo test --locked -p podmesh-integration-tests --features podman-tests
```

The normal CI workflow exposes an optional `run_podman` input for this real-container path. Default
workspace tests and a feature-enabled compile check do not claim actual Podman or multi-host success.
This greenfield PoC has no production latency, availability or recovery SLO; see the
[scope and trust boundaries](../docs/poc-scope.md).
