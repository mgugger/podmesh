# podmesh-proxy

## ADDED Requirements

### Requirement: Proxy announces itself as provider in the DHT

The proxy MUST publish itself under a well-known DHT key so sidecars can discover it.

#### Scenario: Proxy provider announcement
- Given the proxy starts with `--enable-proxy-provider`
- When the libp2p node connects to the DHT
- Then it announces itself as a Kademlia provider for the key `podmesh-proxy-node`
- And sidecars performing a DHT provider lookup for `podmesh-proxy-node` can discover this peer

### Requirement: Proxy accepts and stores sidecar route registrations

The proxy MUST accept `SidecarRegistration` messages from sidecars and store route mappings.

#### Scenario: Sidecar registers its routes
- Given a sidecar sends a `SidecarRegistration` via `/podmesh/sidecar-registration/1.0.0`
- When the proxy processes it
- Then it verifies `sig` is an Ed25519 signature over `manifest_id || sidecar_peer_id`
  using `owner_pubkey` from the registration
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

### Observation: SidecarRegistration owner_pubkey is self-asserted

The proxy verifies the registration signature using `owner_pubkey` from the message itself.
There is no check against a known owner pubkey or any trust anchor. Any peer can register
arbitrary routes by generating a new keypair and signing with it.

### Observation: Egress tunneling has no destination allowlist

The proxy connects to whatever `dest_host:dest_port` a sidecar requests. There is no observed
per-manifest egress policy enforcement in the proxy binary at this time (policy test fixtures
exist but enforcement is in the scheduler/sidecar layer).

### Observation: Ingress routing trusts the registration map without TTL

Sidecar registrations are stored but there is no observed TTL or expiry mechanism.
A stale registration for a terminated workload could persist and misdirect traffic.

### Observation: Proxy DHT discovery is unauthenticated

Sidecars discover the proxy via DHT key `podmesh-proxy-node`. A compromised or malicious DHT
record could redirect sidecar registrations and egress tunnels to an attacker-controlled peer.
