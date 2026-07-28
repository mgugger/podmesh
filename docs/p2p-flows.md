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

The records are self-signed and self-expiring, so an attacker on the HTTP path can only cause a
verification failure, never an impersonation.

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

## Identity And Discovery

A signed EndpointRecord contains an Iroh EndpointId, one relay hint, bounded direct socket address
hints, issue time, and expiry. Proxies refresh their own records before expiry and exchange them over
transport-bound signed announcement streams. Proxy discovery remains tenant-scoped and bounded.

The application authorization behavior is unchanged:

1. The sidecar opens an Iroh connection to a configured EndpointRecord.
2. The existing signed handshake is bound to the authenticated remote EndpointId.
3. The proxy returns the owner-signed Biscuit grant it holds for that tenant, re-verified and
   evicted if expired. The grant store is bounded.
4. The sidecar verifies the grant's tenant owner against the owner key it was injected with, the
   proxy endpoint binding, the signature, and the expiry, allowing bounded clock skew.
5. Only verified proxies receive sidecar registration, discovery, and egress streams.
6. The proxy verifies sidecar registration signature, tenant owner, endpoint binding, and expiry.

Grants are Biscuit tokens rather than opaque certificates so a proxy can later attenuate and
delegate its authority without the owner reissuing. They authenticate only the proxy-to-sidecar
relationship; external ingress clients never present one.

Route registration remains the only ingress routing authority. Routes expire after 120 seconds and
are refreshed every 30 seconds. Ingress fails closed when no live route or connection exists.

## Deployment Data Boundary

Podctl accepts base64 signed EndpointRecords from `podmesh.io/proxy-endpoints` or
`PODMESH_PROXY_ENDPOINTS`. The workload relay token comes from
`PODMESH_WORKLOAD_RELAY_AUTH_TOKEN`; optional private CA certificates come from
`PODMESH_WORKLOAD_RELAY_CA_CERTS` as base64 DER values.

When those are not supplied, `PODMESH_PROXY_URL` bootstraps all three from the proxies' REST APIs
via `GET /api/v1/workload_relay_bootstrap`. Every listed proxy must report the same relay token,
because a sidecar is injected with exactly one; proxies achieve that by adopting a peer's token
rather than each minting its own. Podctl also mints one owner-signed Biscuit grant per proxy and
posts it to `POST /api/v1/proxy_grant` before deploying.

Podctl puts these values only in the encrypted owner-signed execution specification. The selected
agent injects them into sidecar metadata. They are not sent to the scheduler or the machine relay.

## Availability

A scheduler restart does not affect running workloads. A proxy relay outage interrupts relay-only
paths to that proxy, while established direct paths may continue. Configure several regional proxy
EndpointRecords for redundancy. Remote recovery after loss of an agent and its durable keys remains
out of scope.
