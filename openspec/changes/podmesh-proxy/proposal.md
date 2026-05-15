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
- Kademlia DHT (**server mode**): announces proxy as a provider under `podmesh-proxy-node`
- `/podmesh/ingress-proxy/1.0.0` (outbound): sends `ProxyHttpRequest` to sidecar peers
- `/podmesh/egress-tunnel/1.0.0` (inbound): accepts tunnel streams from sidecars
- `/podmesh/sidecar-registration/1.0.0` (inbound): receives `SidecarRegistration` from sidecars

### HTTP Ingress Server

Source: `src/ingress.rs`

Listens for external HTTP requests. Enabled with `--enable-ingress`.

## Observed Behavior

### Proxy Provider Announcement

- When `--enable-proxy-provider` is set, the proxy announces itself in the DHT under
  the key `podmesh-proxy-node`.
- Sidecars discover the proxy by doing a DHT lookup for this key.

### Sidecar Registration Handling

Source: `src/p2p.rs`

- On receiving a `SidecarRegistration` via `/podmesh/sidecar-registration/1.0.0`:
  1. Extracts `manifest_id`, `routes`, `sidecar_peer_id`, `owner_pubkey`, `sig`.
  2. Verifies `sig` = Ed25519 signature over `manifest_id || sidecar_peer_id`
     using `owner_pubkey`. NOTE: There is no check that `owner_pubkey` matches a known
     trusted owner — any key pair can sign a valid registration.
  3. Stores the route mapping: hostname/path → sidecar peer_id.

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

- The proxy verifies `SidecarRegistration.sig` (Ed25519 over `manifest_id || sidecar_peer_id`)
  but only against `owner_pubkey` supplied in the registration message itself.
  Any attacker who can reach the proxy via libp2p can register arbitrary routes with a
  self-generated keypair.
- Ingress requests are forwarded to whatever sidecar peer is registered for a hostname.
  There is no mutual authentication between the external HTTP client and the proxy.
- Egress tunnel connections are made to whatever `dest_host:dest_port` the sidecar requests.
  There is no egress allowlist or policy enforcement in the current implementation
  (policy-enforcing manifests exist in test fixtures but enforcement is not confirmed in proxy).
- The proxy trusts the DHT for discovering sidecars — a compromised DHT record could redirect
  ingress traffic to a malicious peer.

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
