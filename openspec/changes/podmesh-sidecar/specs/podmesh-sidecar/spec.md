# podmesh-sidecar

> This component spec reflects current sidecar behavior.
> Sidecar/proxy tenant-auth requirements are defined in
> `changes/sidecar-proxy-auth/specs/sidecar-proxy-auth/spec.md`.

## ADDED Requirements

### Requirement: Sidecar reads startup metadata from file or environment variable

The sidecar MUST load its configuration from `SidecarMetadata` at startup.

#### Scenario: Metadata loaded from file
- Given `/var/run/podmesh/sidecar/metadata.json` exists and contains valid `SidecarMetadata` JSON
- When the sidecar process starts
- Then it loads manifest_id, manifest_b64, owner_public_key_b64, and bootstrap_peer from the file

#### Scenario: Metadata loaded from environment variable
- Given the environment variable `PODMESH_SIDECAR_METADATA_B64` is set to base64-encoded `SidecarMetadata` JSON
- When the sidecar process starts
- Then it decodes and parses the variable instead of reading the file

### Requirement: Sidecar announces itself in the workload DHT

The sidecar MUST publish its presence and manifest record in the Kademlia DHT.

#### Scenario: Sidecar announces on startup
- Given the sidecar has dialed the bootstrap_peer successfully
- When bootstrap completes
- Then the sidecar announces itself as a Kademlia provider for `manifest_id`
- And it publishes a signed manifest record to DHT key `podmesh/manifest/{manifest_id}`
- And the record is periodically refreshed while the sidecar is running

### Requirement: Sidecar discovers proxy using tenant-derived DHT key

The sidecar MUST use the tenant-derived proxy key for workload traffic.

#### Scenario: Tenant proxy lookup
- Given `owner_public_key_b64` is present in SidecarMetadata
- When the sidecar seeks a proxy
- Then it computes `blake3(owner_pubkey_bytes)[..16]`
- And performs provider lookup using that key
- And it does not use `podmesh-proxy-node` for workload registration/egress routing

### Requirement: Sidecar registers routes with a verified proxy

The sidecar MUST send `SidecarRegistration` only after proxy cert verification.

#### Scenario: Sidecar registers with proxy
- Given a proxy peer has been discovered from the tenant-derived DHT key
- And the sidecar has verified the proxy NodeCert from handshake response
- When the sidecar completes handshake validation
- Then it sends a `SidecarRegistration` via `/podmesh/sidecar-registration/1.0.0`
- And `SidecarRegistration.sig` is an Ed25519 signature over `manifest_id || sidecar_peer_id`
- And it includes `sidecar_signing_pubkey` and routes extracted from the manifest

### Requirement: Sidecar forwards ingress requests to the local application

The sidecar MUST proxy inbound `ProxyHttpRequest` messages to the local app process.

#### Scenario: Ingress request received
- Given a proxy peer sends a `ProxyHttpRequest` on `/podmesh/ingress-proxy/1.0.0`
- When the sidecar receives it
- Then it issues an HTTP request to `http://127.0.0.1:{app_port}{path}`
- And it returns a `ProxyHttpResponse { status, headers, body }` to the caller

### Requirement: Sidecar tunnels egress traffic through the proxy

The sidecar MUST relay outbound TCP connections through a proxy peer via libp2p streams.

#### Scenario: Transparent egress (CAP_NET_ADMIN mode)
- Given the sidecar has `CAP_NET_ADMIN` and transparent mode is enabled
- And nftables rules redirect outbound TCP to the sidecar's egress port
- When the local app makes an outbound TCP connection
- Then the sidecar intercepts it via `SO_ORIGINAL_DST`
- And opens a `/podmesh/egress-tunnel/1.0.0` stream to a verified proxy peer
- And bidirectionally copies data between the local socket and the stream

#### Scenario: HTTP CONNECT egress proxy mode
- Given HTTP CONNECT mode is enabled
- And the app uses `http_proxy=http://127.0.0.1:{port}`
- When the app issues a CONNECT request
- Then the sidecar resolves the target and opens a tunnel via a verified proxy peer as above

## OBSERVED Trust Gaps

### Observation: SidecarMetadata file is not integrity-protected

The metadata file at `/var/run/podmesh/sidecar/metadata.json` has no cryptographic signature.
Any process with write access to the path can inject arbitrary manifest_id, manifest, or bootstrap_peer values.

### Observation: Ingress proxy protocol has no per-request authentication

Any libp2p peer that connects to the sidecar and speaks `/podmesh/ingress-proxy/1.0.0`
can send arbitrary HTTP requests to the local application. There is no token or allowlist check.
