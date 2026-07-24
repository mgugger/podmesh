# podmesh-proxy

> This component spec reflects current proxy behavior.
> Sidecar/proxy tenant-auth requirements are defined in
> `changes/sidecar-proxy-auth/specs/sidecar-proxy-auth/spec.md`.

## ADDED Requirements

### Requirement: Proxy announces itself in the DHT

The proxy MUST announce itself for workload-authenticated discovery using a tenant-derived DHT key.

#### Scenario: Tenant-derived provider announcement
- Given the proxy is configured with owner pubkey material (or receives a tenant `NodeCert`)
- When it computes the tenant key
- Then it announces itself as a provider under `blake3(owner_pubkey_bytes)[..16]`
- And sidecars use this key for workload-authenticated proxy discovery

### Requirement: Proxy accepts and stores sidecar route registrations

The proxy MUST accept `SidecarRegistration` messages from sidecars and store route mappings.

#### Scenario: Sidecar registers its routes
- Given a sidecar sends a `SidecarRegistration` via `/podmesh/sidecar-registration/1.0.0`
- When the proxy processes it
- Then it verifies `sig` is an Ed25519 signature over `manifest_id || sidecar_peer_id`
  using `sidecar_signing_pubkey`
- And it verifies transport `peer_id == sidecar_peer_id`
- And it verifies the registration `owner_pubkey` matches a stored tenant `NodeCert`
- And it stores the mapping: manifest_id → sidecar_peer_id + routes

### Requirement: Proxy routes external HTTP requests to the correct sidecar

The proxy MUST resolve incoming requests to the correct sidecar and forward them.

#### Scenario: Ingress HTTP request routing
- Given an HTTP request arrives with `Host: {name}.mesh.local`
- When the proxy processes it
- Then it derives `manifest_id` by stripping the `.mesh.local` suffix from the hostname
- And it looks up the registered sidecar peer for that `manifest_id`
- And it sends a `ProxyHttpRequest` to the sidecar via `/podmesh/ingress-proxy/1.0.0`
- And it returns the `ProxyHttpResponse` to the original HTTP client

### Requirement: Proxy relays egress TCP tunnels for sidecars

The proxy MUST accept egress tunnel streams and relay them to the requested destination.

#### Scenario: Egress tunnel relay
- Given a sidecar opens a `/podmesh/egress-tunnel/1.0.0` libp2p stream
- And sends `EgressTunnelRequest { dest_host, dest_port, protocol=TCP }`
- When the proxy processes it
- Then it opens a TCP connection to `dest_host:dest_port`
- And responds with `EgressTunnelResponse { success: true }`
- And bidirectionally copies data between the libp2p stream and the TCP socket

## OBSERVED Trust Gaps

### Observation: Egress tunneling has no destination allowlist

The proxy connects to whatever `dest_host:dest_port` a sidecar requests. There is no observed
per-manifest egress policy enforcement in the proxy binary at this time (policy test fixtures
exist, but destination policy is not currently enforced on this path).

### Observation: Ingress routing trusts the registration map without TTL

Sidecar registrations are stored but there is no observed TTL or expiry mechanism.
A stale registration for a terminated workload could persist and misdirect traffic.

### Observation: Proxy cert storage is in-memory

Tenant `NodeCert` entries are currently held in an in-memory map. Durable persistence
across proxy restarts is not implemented yet.
