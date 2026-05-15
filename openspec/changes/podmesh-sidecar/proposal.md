# podmesh-sidecar: In-Pod Service Mesh Agent

## Purpose

Document the behavior of `podmesh-sidecar` as it exists in the codebase.
This is a descriptive spec — it records observed implementation, not intended design.

## Component Location

`podmesh-sidecar/src/`

## Role

`podmesh-sidecar` is injected into each workload pod by the scheduler after deployment.
It is the mesh agent that handles all inbound and outbound network traffic for the workload.

It does NOT run as a sidecar container in the Kubernetes sense — it is a separate process
injected into the workload's runtime environment. It reads its configuration from a file
or environment variable written by the scheduler.

## Startup and Configuration

Source: `src/main.rs`

Configuration is loaded from one of:
1. File: `/var/run/podmesh/sidecar/metadata.json` (default path)
2. Environment variable: `PODMESH_SIDECAR_METADATA_B64` (base64-encoded `SidecarMetadata` JSON)

`SidecarMetadata` fields:
```
manifest_id          — short blake3-derived hex ID of the workload spec
manifest_b64         — base64-encoded full manifest payload
owner_public_key_b64 — (optional) base64 Ed25519 owner pubkey
bootstrap_peer       — multiaddr of a DHT bootstrap peer (set by scheduler)
```

CLI flags supplement metadata (app_port, libp2p bind, etc.).

## libp2p Protocols

Source: `src/lib.rs`

The sidecar runs a libp2p node (QUIC transport, Noise handshake) with these protocols:

| Protocol | ID | Direction | Description |
|---|---|---|---|
| Kademlia | (standard) | client mode | DHT peer/record lookup |
| Handshake | `/podmesh/handshake/1.0.0` | inbound + outbound | Ed25519/KEM key exchange with peers |
| Ingress proxy | `/podmesh/ingress-proxy/1.0.0` | inbound | Receives `ProxyHttpRequest`, forwards to local app |
| Manifest fetch | `/podmesh/sidecar-manifest/1.0.0` | inbound | Serves manifest bytes on request |
| Egress tunnel | `/podmesh/egress-tunnel/1.0.0` | outbound | Opens stream to proxy for TCP tunneling |
| Sidecar registration | `/podmesh/sidecar-registration/1.0.0` | outbound | Registers routes with proxy |

## Observed Behavior

### DHT Announcement

- On startup, dials bootstrap peer.
- Periodically publishes a manifest record to DHT key `podmesh/manifest/{manifest_id}`.
  The record is signed by the sidecar's Ed25519 key and includes route info.
- Announces itself as a Kademlia provider for `manifest_id`.

### Proxy Registration

Source: `src/lib.rs` (registration_rr)

- After bootstrap completes, the sidecar sends a `SidecarRegistration` to the proxy peer.
- `SidecarRegistration` contains: `manifest_id`, `routes` (Vec<SidecarRoute>), `sidecar_peer_id`,
  `owner_pubkey`, `sig` = Ed25519 signature over `manifest_id || sidecar_peer_id`.
- The proxy peer is discovered via DHT key `podmesh-proxy-node`.

### Ingress Request Handling

- On receiving a `ProxyHttpRequest` via `/podmesh/ingress-proxy/1.0.0`:
  1. Extracts HTTP method, path, headers, body.
  2. Issues an HTTP request to `http://127.0.0.1:{app_port}{path}` (default `app_port = 18080`).
  3. Returns `ProxyHttpResponse { status, headers, body }` to the caller.

### Egress Tunneling

Two modes (source: `src/egress_nft.rs`, `src/egress_proxy.rs`, `src/http_connect_proxy.rs`):

**Transparent mode** (requires `CAP_NET_ADMIN`):
- nftables rules redirect all outbound TCP to the sidecar's egress proxy port.
- `EgressProxy` intercepts connections, reads original destination via `SO_ORIGINAL_DST`.

**HTTP CONNECT mode**:
- Exposes an HTTP CONNECT proxy on a local port.
- App sets `http_proxy=http://127.0.0.1:{port}` explicitly.

Both modes:
- Send an `EgressTunnelRequest { dest_host, dest_port, protocol=TCP }` to the sidecar event loop.
- Sidecar opens a libp2p stream to a known proxy peer via `/podmesh/egress-tunnel/1.0.0`.
- On success, bidirectionally copies between the local socket and the P2P stream.

## Trust Assumptions (as implemented)

- The sidecar trusts `SidecarMetadata` as written by the scheduler. There is no signature
  on the metadata file or env var — a process with write access to the file path can inject
  arbitrary metadata.
- Sidecar registration signature covers only `manifest_id || sidecar_peer_id`. A re-registration
  with the same manifest_id by a different peer is not distinguished from a legitimate one by
  the proxy unless the proxy checks owner_pubkey consistency.
- Ingress requests are forwarded to the local app without authentication — any peer that can
  reach the sidecar's libp2p QUIC address and knows the ingress-proxy protocol can inject requests.
- Egress tunneling trusts the proxy peer to make outbound connections on the sidecar's behalf.
  The sidecar does not verify the proxy peer's identity against a trust anchor.

## Data Flow

```
Scheduler writes SidecarMetadata to /var/run/podmesh/sidecar/metadata.json
  ▼
podmesh-sidecar reads metadata, starts libp2p node
  │ dial bootstrap_peer, join DHT
  │ publish manifest record + announce as provider
  │ discover proxy peer via DHT key "podmesh-proxy-node"
  │ send SidecarRegistration (signed) to proxy
  ▼
Ingress path:
  proxy → ProxyHttpRequest (ingress-proxy/1.0.0) → sidecar
  sidecar → HTTP GET/POST → local app (127.0.0.1:18080)
  sidecar ← HTTP response ← local app
  proxy ← ProxyHttpResponse ← sidecar

Egress path:
  local app → outbound TCP (intercepted by nftables or explicit CONNECT)
  sidecar → EgressTunnelRequest → proxy (egress-tunnel/1.0.0 stream)
  proxy → TCP connection to dest_host:dest_port
  bidirectional copy: local ↔ sidecar ↔ proxy ↔ internet
```

## Non-Goals of This Spec

- Does not describe proxy behavior (see podmesh-proxy spec)
- Does not describe workload decryption (see workload-execution spec)
- Does not describe nftables rule management details
