# podmesh-proxy: Ingress and Egress Edge Node

## Purpose

Document the behavior of `podmesh-proxy` as it exists in the codebase.
This is a descriptive spec — it records observed implementation, not intended design.

## Component Location

`podmesh-proxy/src/`

## Role

`podmesh-proxy` is an edge node that:
1. Receives external HTTP traffic and routes it into the mesh (ingress)
2. Acts as an egress relay for workloads that need outbound TCP connections (egress)
3. Maintains a registration of sidecar peers and their route tables

It runs as a standalone binary, separate from `podmesh-scheduler`.

## Interfaces

### REST API (TCP `:7100` by default)

Source: `src/restapi.rs`

Exposes operational endpoints for the proxy node.
(Specific routes not fully enumerated in public surface — implementation-defined.)

### libp2p (QUIC)

Source: `src/p2p.rs`

Protocols:
- Kademlia DHT (**server mode**): tenant-derived announcements under `blake3(owner_pubkey_bytes)[..16]`
- `/podmesh/handshake/1.0.0` (inbound+outbound): handshake; proxy includes `proxy_cert_b64` in responses when tenant cert material is provisioned
- `/podmesh/ingress-proxy/1.0.0` (outbound): sends `ProxyHttpRequest` to sidecar peers
- `/podmesh/egress-tunnel/1.0.0` (inbound): accepts tunnel streams from sidecars
- `/podmesh/sidecar-registration/1.0.0` (inbound): receives `SidecarRegistration` from sidecars

### HTTP Ingress Server

Source: `src/ingress.rs`

Listens for external HTTP requests. Enabled with `--enable-ingress`.

## Observed Behavior

### Proxy Provider Announcement

- When owner pubkey / tenant cert material is available, the proxy announces itself
  under tenant-derived key bytes `blake3(owner_pubkey_bytes)[..16]`.
- Sidecars performing workload-authenticated traffic discovery use this tenant-derived key.

### Sidecar Registration Handling

Source: `src/p2p.rs`

- On receiving a `SidecarRegistration` via `/podmesh/sidecar-registration/1.0.0`:
  1. Extracts `manifest_id`, `routes`, `sidecar_peer_id`, `owner_pubkey`, `sig`, `sidecar_signing_pubkey`.
  2. Verifies `sig` = Ed25519 signature over `manifest_id || sidecar_peer_id`
     using `sidecar_signing_pubkey`.
  3. Verifies transport peer identity: connection `peer_id == sidecar_peer_id`.
  4. Verifies `owner_pubkey` matches a stored tenant `NodeCert` and that cert is not expired.
  5. Stores the route mapping: hostname/path → sidecar peer_id.

### Ingress Request Routing

Source: `src/ingress.rs`

- Receives HTTP request with `Host: {name}.mesh.local` (or configured domain).
- Strips the mesh domain suffix to derive `manifest_id` (e.g. `demo-nginx.mesh.local` → `demo-nginx`).
- Looks up the sidecar peer for `manifest_id` (from in-memory registration map or DHT).
- Sends `ProxyHttpRequest` to the sidecar via `/podmesh/ingress-proxy/1.0.0`.
- Awaits `ProxyHttpResponse` and returns it as the HTTP response to the external caller.

### Egress Tunnel Handling

Source: `src/p2p.rs`, `src/workload.rs`

- On receiving an `/podmesh/egress-tunnel/1.0.0` stream from a sidecar:
  1. Reads `EgressTunnelRequest { dest_host, dest_port, protocol=TCP }` (postcard, length-prefixed LE u32).
  2. Opens a TCP connection to `dest_host:dest_port`.
  3. Sends `EgressTunnelResponse { success: true/false }`.
  4. If successful, bidirectionally copies between the libp2p stream and the TCP socket.

## Trust Assumptions (as implemented)

- Ingress requests are forwarded to whatever sidecar peer is registered for a hostname.
  There is no mutual authentication between the external HTTP client and the proxy.
- Egress tunnel connections are made to whatever `dest_host:dest_port` the sidecar requests.
  There is no egress allowlist or policy enforcement in the current implementation.
- Tenant `NodeCert` material used for registration checks is currently held in-memory (not durable across restarts).

## Data Flow

```
External HTTP client
  │ GET http://demo-nginx.mesh.local/index.html
  ▼
podmesh-proxy (ingress, port 80 or configured)
  │ parse hostname → manifest_id = "demo-nginx"
  │ lookup sidecar peer (registration map or DHT)
  │ ProxyHttpRequest → sidecar peer via /podmesh/ingress-proxy/1.0.0
  ▼
podmesh-sidecar (in workload pod)
  │ HTTP GET http://127.0.0.1:18080/index.html
  ▼
local application
  ← HTTP 200 response
  ← ProxyHttpResponse
  ← HTTP 200 to external client

Egress path:
podmesh-sidecar → /podmesh/egress-tunnel/1.0.0 stream
  │ EgressTunnelRequest { dest_host="api.example.com", dest_port=443 }
  ▼
podmesh-proxy
  │ TCP connect to api.example.com:443
  │ EgressTunnelResponse { success: true }
  │ bidirectional copy: sidecar stream ↔ TCP socket
```

## Non-Goals of This Spec

- Does not describe sidecar behavior (see podmesh-sidecar spec)
- Does not describe egress policy enforcement in depth (not confirmed as implemented)
- Does not describe TLS termination at ingress (not observed in implementation)
