# Sidecar–Proxy Mutual Authentication and Tenant Binding

## Context

This spec addressed the original trust gap between `podmesh-sidecar` and `podmesh-proxy`:
authenticated libp2p QUIC/TLS transport did not prove tenant membership, and the original global
proxy discovery key was tenant-agnostic.

The trust anchor for workload ownership in podmesh is the operator's Ed25519 keypair
(managed by podctl). This spec extends that anchor to proxy identity by:

1. Having the proxy announce itself under an obfuscated per-tenant DHT key.
2. Having the proxy present a tenant-signed `NodeCert` during connection handshake.
3. Having the sidecar verify that cert before proceeding.
4. Having the proxy verify the sidecar's workload owner at registration time.

`SidecarMetadata` is NOT changed — the sidecar already receives `owner_public_key_b64`,
which is sufficient as the tenant anchor for both DHT key derivation and cert verification.

---

## ADDED Requirements

### Requirement: podctl provisions a tenant-signed NodeCert before proxy use

The operator MUST run `podctl cert grant-proxy` to sign and deliver a `NodeCert` to the proxy
before any workload that uses that proxy is deployed. Re-provisioning renews or replaces the
in-memory certificate for the tenant–proxy pair and is required after proxy restart until durable
certificate storage exists. The cert is delivered directly to the proxy's REST API; it is not
embedded in any workload submission or `SidecarMetadata`.

#### Scenario: podctl fetches proxy key material and signs a cert
- Given the operator runs `podctl cert grant-proxy --proxy-url <url> --owner-pub <path> --owner-sk <path> [--ttl-days <days>]`
- And the operator's Ed25519 owner keypair bytes are loaded from those key files
- When podctl executes
- Then it retrieves the proxy's `signing_pubkey` from `GET <url>/api/v1/signing_pubkey`
- And retrieves the proxy's `kem_pubkey` from `GET <url>/api/v1/kem_pubkey`
- And retrieves or derives the proxy's libp2p `peer_id`
- And constructs a `NodeCert` with:
  - `peer_id` = proxy's libp2p PeerId
  - `signing_pubkey` = proxy's Ed25519 public key (base64)
  - `kem_pubkey` = proxy's X25519 public key (base64)
  - `capabilities` = `["proxy"]`
  - `role` = `NodeRole::Proxy`
  - `valid_until` = `now_unix_secs + ttl_secs`
  - `owner_pubkey` = operator's Ed25519 public key (base64)
  - `owner_sig` = `Ed25519.sign(owner_sk, canonical_bytes(NodeCert))`
- And POSTs the base64 postcard-encoded cert to `POST <url>/api/v1/node_cert`
- And the proxy responds with a success acknowledgement
- And podctl prints the obfuscated DHT key `hex(blake3(owner_pubkey_bytes)[..16])` that
  the proxy will announce under

### Requirement: Proxy exposes a REST endpoint to receive a tenant-signed NodeCert

The proxy MUST accept cert delivery via its REST API and store the cert durably,
keyed by `owner_pubkey`, so it survives restarts.

#### Scenario: Proxy receives and stores a NodeCert
- Given a POST request arrives at `/api/v1/node_cert` with a base64 postcard `NodeCert` body
- When the proxy processes it
- Then it deserializes the `NodeCert`
- And verifies `NodeCert.verify()` — `owner_sig` is a valid Ed25519 sig by `owner_pubkey`
  over `canonical_bytes(NodeCert)`
- And verifies `NodeCert.peer_id` matches the proxy's own libp2p PeerId
- And verifies `NodeCert.role == NodeRole::Proxy`
- And verifies `NodeCert.is_expired()` is false
- And if all checks pass, stores the cert durably keyed by `owner_pubkey`
- And responds with HTTP 200 and a success acknowledgement

#### Scenario: Proxy rejects a cert for a different peer_id
- Given a `NodeCert` is posted where `peer_id` does not match the proxy's own PeerId
- When the proxy processes it
- Then it responds with an error and does not store the cert

#### Scenario: Proxy replaces an existing cert for the same owner on re-provisioning
- Given the proxy already holds a `NodeCert` for owner O
- And a new `NodeCert` for the same owner O is posted with a later `valid_until`
- When the proxy processes it
- Then it replaces the stored cert with the new one
- And re-announces under the same obfuscated DHT key (no change to the key)

### Requirement: Proxy announces itself under an obfuscated per-tenant DHT key

The proxy MUST announce its presence in the DHT using a key derived from the tenant's
owner pubkey such that an observer of the DHT cannot recover the owner pubkey.

#### Scenario: Proxy DHT announcement for a tenant
- Given the proxy holds a valid `NodeCert` issued by owner pubkey O
- When the proxy starts or the cert is loaded
- Then it computes `proxy_dht_key = hex(blake3(owner_pubkey_bytes)[..16])`
  where `owner_pubkey_bytes` is the raw decoded bytes of `NodeCert.owner_pubkey`
- And it announces itself as a Kademlia provider for `proxy_dht_key`
- And it does NOT announce under `podmesh-proxy-node` for workload-authenticated traffic

#### Scenario: Proxy holding multiple tenant certs
- Given the proxy holds NodeCerts for tenants O1 and O2
- When it announces in the DHT
- Then it announces separately under `hex(blake3(O1_bytes)[..16])` and `hex(blake3(O2_bytes)[..16])`

### Requirement: Sidecar discovers proxy using the obfuscated tenant DHT key

The sidecar MUST derive the same DHT key from its own `owner_public_key_b64` to find
the correct proxy, and MUST NOT use the global `podmesh-proxy-node` key for workload traffic.

#### Scenario: Sidecar proxy discovery
- Given the sidecar has loaded `SidecarMetadata` with `owner_public_key_b64` = O
- When the sidecar seeks a proxy for its workload
- Then it computes `proxy_dht_key = hex(blake3(O_bytes)[..16])`
- And performs a Kademlia provider lookup for `proxy_dht_key`
- And uses the resulting peer(s) as proxy candidates

#### Scenario: Sidecar does not fall back to unauthenticated proxy discovery
- Given no provider is found under the obfuscated tenant DHT key
- When the sidecar cannot find a proxy
- Then it does NOT fall back to looking up `podmesh-proxy-node`
- And it logs an error and waits to retry

### Requirement: Proxy presents its NodeCert in the handshake response

The proxy MUST include its tenant-signed `NodeCert` as an extension in the
`/podmesh/handshake/1.0.0` response so the sidecar can verify it.

#### Scenario: Proxy handshake response includes cert
- Given a peer sends a `/podmesh/handshake/1.0.0` request to the proxy
- When the proxy builds its handshake response
- Then it includes `proxy_cert_b64` (base64 postcard `NodeCert`) in the signed envelope
- And non-proxy nodes leave this field absent (the field is optional)

### Requirement: Sidecar verifies the proxy NodeCert after handshake

The sidecar MUST extract and verify the proxy's `NodeCert` from the handshake response
before sending `SidecarRegistration` or accepting any ingress request.

#### Scenario: Sidecar verifies a valid proxy cert
- Given the sidecar has completed the handshake with a proxy candidate
- And the handshake response contains `proxy_cert_b64`
- When the sidecar processes the response
- Then it deserializes the `NodeCert`
- And verifies all of the following:
  1. `NodeCert.verify()` passes — `owner_sig` is a valid Ed25519 sig by `NodeCert.owner_pubkey`
     over `canonical_bytes(NodeCert)`
  2. `NodeCert.owner_pubkey` equals `SidecarMetadata.owner_public_key_b64` — same tenant
  3. `NodeCert.is_expired()` is false — `valid_until > now_unix_secs`
  4. The handshake signer's peer_id (from the signed envelope) equals `NodeCert.peer_id`
- And only if all checks pass does it proceed to send `SidecarRegistration`

#### Scenario: Sidecar rejects proxy with no cert in handshake
- Given the proxy handshake response contains no `proxy_cert_b64`
- When the sidecar checks for the cert
- Then it closes the connection
- And does not attempt to register with that peer

#### Scenario: Sidecar rejects proxy cert signed by a different owner
- Given `NodeCert.owner_pubkey` is key X
- And `SidecarMetadata.owner_public_key_b64` is key O (X ≠ O)
- When the sidecar verifies the cert
- Then it rejects the connection (different tenant)

#### Scenario: Sidecar rejects an expired proxy cert
- Given `NodeCert.valid_until` is less than the current unix timestamp
- When the sidecar verifies the cert
- Then it rejects the connection

#### Scenario: Sidecar rejects proxy where cert peer_id does not match transport identity
- Given `NodeCert.peer_id` is "QmProxy1"
- And the handshake envelope was signed by a key corresponding to PeerId "QmProxy2"
- When the sidecar verifies the cert
- Then it rejects the connection (cert does not belong to this peer)

### Requirement: SidecarRegistration includes the sidecar's own Ed25519 signing public key

The proxy MUST be able to verify the registration signature without trusting key material
from a prior unauthenticated message.

#### Scenario: SidecarRegistration carries sidecar signing pubkey
- Given a sidecar sends `SidecarRegistration` after a verified handshake
- Then the message contains `sidecar_signing_pubkey` (base64 Ed25519 public key)
- And `sig` is `Ed25519.sign(sidecar_sk, manifest_id || sidecar_peer_id)`

### Requirement: Proxy verifies SidecarRegistration against the tenant owner key

The proxy MUST reject registrations from sidecars whose workload owner does not match
the tenant in the proxy's `NodeCert`.

#### Scenario: Proxy accepts registration from same-tenant sidecar
- Given the proxy holds a `NodeCert` with `owner_pubkey` O
- And a sidecar sends `SidecarRegistration { owner_pubkey: O, sidecar_signing_pubkey: S, sig }`
- When the proxy processes the registration
- Then it verifies:
  1. `sig` is a valid Ed25519 sig over `manifest_id || sidecar_peer_id` using `sidecar_signing_pubkey` S
  2. `owner_pubkey` O matches the `owner_pubkey` in the proxy's `NodeCert`
  3. The transport peer_id of the connection equals `sidecar_peer_id`
- And if all checks pass, stores the route mapping: manifest_id → sidecar_peer_id + routes

#### Scenario: Proxy rejects registration from a different-tenant sidecar
- Given the proxy holds a `NodeCert` with `owner_pubkey` O
- And a sidecar sends `SidecarRegistration { owner_pubkey: X }` where X ≠ O
- When the proxy processes the registration
- Then it rejects it and does not store any route mapping

#### Scenario: Proxy rejects registration with invalid sidecar signature
- Given a sidecar sends a `SidecarRegistration` where `sig` does not verify
  against `sidecar_signing_pubkey`
- When the proxy processes the registration
- Then it rejects it

#### Scenario: Proxy rejects registration where transport peer_id differs from claimed peer_id
- Given the libp2p connection's remote PeerId is "QmReal"
- And `SidecarRegistration.sidecar_peer_id` is "QmFake"
- When the proxy processes the registration
- Then it rejects it

### Requirement: Proxy only relays traffic from verified sidecars

The proxy MUST refuse ingress forwarding and egress tunnel relay for any peer whose
`SidecarRegistration` has not been successfully verified.

#### Scenario: Proxy forwards ingress only to verified sidecar
- Given an HTTP ingress request arrives for manifest_id M
- When the proxy looks up the registered sidecar for M
- Then it only sends `ProxyHttpRequest` to a peer that passed all registration checks
- And returns an error to the caller if no verified sidecar is registered for M

#### Scenario: Proxy refuses egress tunnel from unregistered peer
- Given a libp2p stream arrives on `/podmesh/egress-tunnel/1.0.0`
- And the connecting peer_id has no verified registration in the proxy's route map
- When the proxy receives the stream
- Then it sends `EgressTunnelResponse { success: false }` and closes the stream

---

## CHANGED Requirements

### Requirement: Proxy no longer uses self-asserted owner_pubkey as trust anchor

The previous behavior — verifying `SidecarRegistration.sig` using `owner_pubkey` taken
from the registration message itself — is insufficient and MUST NOT be the sole check.

#### Scenario: Registration with self-asserted owner is insufficient
- Given a proxy receives `SidecarRegistration { owner_pubkey: X, sig }` where sig is
  valid under X
- When the proxy evaluates the registration
- Then this alone is NOT sufficient for acceptance
- And the proxy MUST also verify X against its own `NodeCert.owner_pubkey`

### Requirement: Sidecar no longer uses the global proxy-node DHT key for workload traffic

The `podmesh-proxy-node` key MUST NOT be used by sidecars to discover an authenticated
proxy for any workload protocol (ingress, egress, registration).

#### Scenario: Global DHT key not used for workload traffic
- Given the sidecar is seeking a proxy for its workload
- When it performs DHT discovery
- Then it uses only the obfuscated tenant-derived key `hex(blake3(owner_pubkey_bytes)[..16])`
- And does not perform a provider lookup under `podmesh-proxy-node` for this purpose
