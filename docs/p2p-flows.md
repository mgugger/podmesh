# Podmesh Network Flows

Podmesh uses Iroh for both network planes while keeping their trust and relay configuration
separate.

## Machine Plane

```text
scheduler <== authenticated Iroh gossip ==> scheduler
agent ===== persistent capacity streams ===> scheduler
podctl ---- HTTP placement request --------> scheduler
podctl ==== HTTP encrypted lifecycle ======> scheduler ==Iroh==> selected agent
```

`podctl` is a plain CLI. It has no Iroh endpoint and cannot be dialed, so it speaks HTTP to any
reachable scheduler. The scheduler answers placement from the mesh and then relays the owner's
already-encrypted control payload to the selected agent over `/podmesh/agent-control/1`. It moves
opaque bytes it can neither read nor forge.

An agent holds exactly one attachment, so the scheduler a client happens to reach usually is not the
one holding it. That scheduler broadcasts a single signed location query on the gossip mesh and only
the holder answers, directly, over the control relay protocol. The query carries no payload, names a
reply endpoint that must belong to its own signer, and expires. The answer is cached with a bounded
lifetime and forgotten as soon as a forward through it fails, so a burst of operations against one
agent resolves once and a moved agent is noticed quickly. Locating an agent therefore costs one
broadcast rather than one connection per scheduler, which is what allows the mesh to be large.

Every scheduler supervises a machine `iroh-relay`. Machine grants admit only scheduler and agent
EndpointIds. Relays forward encrypted QUIC packets and never receive workload plaintext or
application keys.

Schedulers retain only bounded pending queries, offers, and attachment sessions. Agents remain
authoritative for admission, reservations, execution, persistence, status, logs, and deletion.

### HTTP Bootstrap

Iroh endpoints have to be learned before they can be dialed, so both the scheduler mesh and the
agent attachment bootstrap over plain HTTP:

- `GET /api/v1/endpoint_record` on a scheduler returns its signed, expiring `EndpointRecord`, its
  endpoint id, and its signing public key.
- Agents resolve `PODMESH_AGENT_SCHEDULER_URLS` through that endpoint before attaching. A URL that
  does not answer fails startup.
- Schedulers poll `PODMESH_SCHEDULER_PEER_URLS` in the background, skipping their own record, and
  add each verified peer to the gossip member allowlist and the relay trusted issuer set at
  runtime. Both registries are bounded. Start order therefore does not matter, and an unreachable
  peer is a normal transient condition rather than an error.
- Schedulers also announce their own signed record on the gossip mesh at a fixed interval. A
  scheduler that hears an announcement admits the announcer into its member allowlist, which is what
  lets a mesh grow past the peer URLs configured on any single node. This is transitive but still
  vouched for: an announcement can only be heard from a scheduler that some operator-configured
  member already admitted. It grants membership only, never relay-issuer trust.

These records are self-signed: the key that validates one is carried inside it. Verifying a record
therefore proves it is internally consistent and nothing more — whoever answers the URL picks the
identity. HTTP discovery is safe only because identity is pinned separately:

- A scheduler binds each peer URL to one endpoint id and signing key, configured up front or
  remembered from the first observation. A later mismatch is refused and logged, so a party that can
  answer a peer URL cannot substitute itself for an admitted scheduler, and cannot exhaust the member
  or issuer registries by cycling identities.
- An agent's bootstrap URLs only supply addresses; the scheduler still has to authenticate its Iroh
  connection, and an agent answers capacity queries only over that authenticated attachment.

## Workload Plane

```text
sidecar ===== Iroh connection =====> proxy endpoint
          \=== relay fallback ====> proxy-hosted workload relay

proxy  -- opens ingress stream --> sidecar --> local application
sidecar -- opens egress stream --> proxy --> destination
sidecar -- registration stream --> proxy route table
sidecar -- discovery stream ----> proxy EndpointRecords
proxy  -- signed announcement --> connected regional proxy
```

The workload ALPN is `/podmesh/workload/1`. Every bidirectional stream starts with a bounded
operation frame identifying handshake, registration, proxy discovery, proxy announcement, ingress,
or egress. Egress switches to raw bounded byte forwarding only after the existing tunnel request and
response succeed.

Each proxy loads a persistent Iroh secret and supervises an authenticated TLS `iroh-relay` service.
Each sidecar is an ordinary Iroh endpoint: it does not host a relay, join gossip, publish to a DHT,
or participate in scheduler protocols. Iroh may migrate proxy-sidecar connections from relay to a
direct path when connectivity allows.

## Workload Connector Boundary

The networking role runs **inside the workload execution boundary**: today it is the
injected Podman sidecar; a future VM backend should run the same connector as a guest service.
A host component provides attachment and lifecycle transport, not workload traffic termination.
No VM/libkrun backend or hardware attestation is implemented yet. An external host-side connector
would remain a trusted-host mode, not confidential guest networking.

```text
podctl -- owner-signed encrypted command --> scheduler --> agent
                                                         |
                                                WorkloadRuntime
                                                         |
                                   Podman exec, no shell/general command API
                                                         |
                                    sidecar connector-control adapter
                                                         |
                                    private in-workload Unix socket
```

`podctl get connectors <deployment>` inspects every replica through this path. Each response
contains an expiring EndpointRecord and a signature from its Iroh transport key binding the
application signing key, replica identity and a fresh owner-command challenge. Both the agent and
podctl verify the response. Enrolled reports also carry their public endpoint-bound credential chain.
No workload/issuer signing secrets, transport secrets or relay admission tokens are returned.

**A self-signature alone is not enrollment.** Reports without a credential are pending; their
signatures prove possession only. An enrolled report must additionally verify its owner-rooted chain,
subject keys, scope and current validity before it can authorize traffic. Expired certificates can
still be inspected to diagnose/renew a workload but cannot authorize a session. Owner-signed Service
catalogs now enable workload-local DNS and direct tenant peer discovery/routing after enrollment;
single-host rootless Podman DNS/kernel acceptance has passed; rootful and multi-host checks remain pending.

The control socket is enabled explicitly with `PODMESH_CONNECTOR_SOCKET` (the agent injects
`/var/run/podmesh/connector/control.sock` into the sidecar only). It is private, bounded, and has
no TCP listener or machine-plane membership. `PODMESH_CONNECTOR_IDENTITY_DIR` optionally selects
persistent connector identity storage. Podman injection selects `/var/lib/podmesh-connector/identity`
in the sidecar's writable layer: keys survive a normal container restart, not deletion/recreation of
that layer. No managed volume or persistent VM backend is provisioned; tenant-authored volumes remain
rejected. Lost keys require an owner-authorized new enrollment generation rather than silent rebinding. The scratch connector binary implements its own adapter
subcommand and requires no shell.

## Identity And Discovery

A signed EndpointRecord contains an Iroh EndpointId, one relay hint, bounded direct socket address
hints, issue time, and expiry. Proxies refresh their own records before expiry and exchange them over
transport-bound signed announcement streams. Proxy discovery remains tenant-scoped and bounded.

Proxy grants retain their existing endpoint binding. Workload authorization now uses a two-Biscuit chain:
owner -> selected agent enrollment issuer -> endpoint-bound workload identity. The handshake flow is:

1. The sidecar opens an Iroh connection to a configured EndpointRecord.
2. The existing signed handshake is bound to the authenticated remote EndpointId.
3. The proxy returns the owner-signed Biscuit grant it holds for that tenant, re-verified and
   evicted if expired. The grant store is bounded.
4. The sidecar verifies the grant's tenant owner against the owner key it was injected with, the
   proxy endpoint binding, the signature, and the expiry, allowing bounded clock skew.
5. Only verified proxies receive sidecar registration, discovery, and egress streams.
6. The proxy checks that the registration names the endpoint the transport authenticated, that its
   routing key is derived from the owner key it presents, its replica matches the child credential,
   and that the proxy already holds a live grant signed by that owner. Both the actual transport key
   and envelope application signing key must match the workload identity.

Both handshake directions travel in a signed envelope that names the sender, the intended recipient,
and the direction, so a captured handshake cannot be forwarded to a third peer and a response cannot
be replayed as a request.

At deploy time, podctl signs a standing enrollment delegation naming the selected issuer key,
owner, route, replica workload ID, revision and generation, and caps child lifetimes to one hour.
It sends that authority inside the encrypted execution specification, not a bearer session token.
The sidecar generates its own identity. Through runtime-local control the agent obtains fresh
transport-key-signed evidence, durably commits that generation's key binding, and issues/installs a
short-lived child credential. Copying either public token does not prove endpoint-key possession.

The agent renews near expiry without podctl or the owner private key. New children cannot outlive a
finite parent; standing authority is explicit, not self-extension of an expired token. Both Biscuits
are verified, scope is intersected, and holder-added facts cannot create issuer authority. Attenuation
checks can narrow an existing child. A connection can refresh authority only for the same bound
workload and signing keys; registration/discovery/egress envelopes must use that authorized signer.
Legacy bearer credentials are refused, with no permissive fallback.

Deleting a workload atomically removes its local enrollment binding and stops honest-agent issuance.
Existing children may remain valid until expiry; a compromised standing issuer is not revoked globally
by deletion. This is scoped delegated trust in the already owner-approved execution agent, not
hostile-host isolation or hardware attestation. Proxy grant renewal remains separate: its current
finite lifetime/manual recovery limitations are not removed by workload child renewal. Coordinated
upgrades require regenerated enrollment metadata; old bearer configuration cannot keep authorizing
sessions.

Route registration is the only ingress routing authority. A hostname serves a workload only if that
workload claimed it, or under the canonical `<routing-key>.mesh.local` form; an unrecognised `Host`
header resolves to nothing. A hostname claimed by one owner is never reassigned to another. Routes
expire after 120 seconds and are refreshed every 30 seconds. Ingress fails closed when no live route
or connection exists.

Egress tunnels are authorised by tenant, not by destination: the connection must have proven an
owner and that owner must hold a live grant for this proxy. Destinations are deliberately not
filtered by address, because podmesh runs across machines whose application parts legitimately live
on private networks. The consequence is that a tenant which granted a proxy can reach whatever that
proxy can reach, including its own services and cloud instance metadata. A destination is resolved
once and only the resolved addresses are dialled, so a name cannot resolve to one host for a check
and another for the connection. Tunnels are bounded in bytes and in time.

## Workload-local Internal DNS

Agent-injected connectors receive owner-signed Service catalogs. `podctl apply` collects signed
connector records and sends bounded peer seeds through owner-authorized agent/runtime control.
The connector refreshes leases, synchronizes neighbors, serves loopback UDP/TCP split DNS and
routes internal TCP through peer-only mutually authenticated Iroh sessions, before external proxy
selection. VIP mappings, sequences and hints persist beside connector identity. Public CNAME
responses are returned without recursive chasing so aliases cannot leak internal lookups upstream.

Production nftables wiring redirects DNS and VIP TCP before UID/network exemptions; marked
upstream sockets avoid resolver loops. The new container-free production test starts the normal
sidecar, installs a seed and proves remote internal TCP without a live proxy, but disables kernel
rules. The subsequent real rootless Podman check also verifies same-UID DNS/VIP interception,
remote TCP/port translation, lease expiry and continued service with all proxies stopped. Peer
connections are bounded and reused; every stream is independently authenticated. Rootful and
multi-host acceptance remain pending. See
[local-dns.md](local-dns.md) for names, commands, storage/runtime boundaries and remaining limitations.

## Deployment Data Boundary

Podctl accepts base64 signed EndpointRecords from `podmesh.io/proxy-endpoints` or
`PODMESH_PROXY_ENDPOINTS`. This tenant's workload relay token comes from
`PODMESH_WORKLOAD_RELAY_AUTH_TOKEN`; optional private CA certificates come from
`PODMESH_WORKLOAD_RELAY_CA_CERTS` as base64 DER values.

When those are not supplied, `PODMESH_PROXY_URL` bootstraps all three from the proxies' REST APIs
via `GET /api/v1/workload_relay_bootstrap?owner=<key>`, which returns only the token derived for the
tenant named in the request. Relay tokens are derived from a mesh secret the workload never receives,
so one tenant's token does not admit another. Every listed proxy must derive the same token for a
tenant; disagreement means they hold different mesh secrets. Proxies share a secret by adopting a
peer's over the separate `GET /api/v1/workload_relay_mesh_secret`, which is proxy-to-proxy only.
Podctl also mints one owner-signed Biscuit grant per proxy and posts it to
`POST /api/v1/proxy_grant` before deploying, and one scoped enrollment delegation per replica generation.

Podctl puts these values only in the encrypted owner-signed execution specification. The selected
agent injects them into sidecar metadata. They are not sent to the scheduler or the machine relay.

## Availability

A scheduler restart does not affect running workloads. A proxy relay outage interrupts relay-only
paths to that proxy, while established direct paths may continue. Configure several regional proxy
EndpointRecords for redundancy. Remote recovery after loss of an agent and its durable keys remains
out of scope.
