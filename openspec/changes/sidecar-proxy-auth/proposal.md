# Sidecar–Proxy Mutual Authentication and Tenant Binding

## Problem

Before this change, sidecar and proxy traffic used authenticated libp2p QUIC/TLS transport but had
no tenant binding beyond transport identity. The implementation addressed these gaps:

- The sidecar discovers a proxy via DHT under the global key `podmesh-proxy-node` and
  connects to it without any way to determine whether the proxy belongs to the same tenant.
- The proxy accepts `SidecarRegistration` from any peer — `owner_pubkey` is self-asserted
  in the message; any peer can claim any owner identity.
- A rogue proxy can register itself in the DHT and intercept sidecar registrations and
  egress tunnel streams from sidecars belonging to arbitrary tenants.
- A rogue sidecar can register fake routes with any proxy.

## Constraints and Corrections vs Previous Proposal

The previous proposal delivered the proxy cert to the sidecar via `SidecarMetadata`.
This is incorrect for two reasons:

1. The sidecar should NOT receive the proxy cert pre-loaded. It must obtain and verify
   the cert during connection establishment, not before.
2. Pre-loading the cert would require podctl to know which proxy will be used at submission
   time, creating unnecessary coupling.

The correct model:
- The proxy announces itself in the DHT under a key derived from the owner pubkey in an
  **obfuscated** (one-way) way — so the sidecar can find the right proxy for its tenant
  without the DHT leaking the owner identity in plaintext.
- The sidecar discovers the proxy via this derived key, connects, and **verifies the proxy's
  NodeCert during connection** (the proxy presents it).
- The proxy also verifies the sidecar's workload owner at registration time.

## Solution

### Obfuscated DHT Announcement

The proxy announces itself in the DHT under a per-tenant key derived as:

```
proxy_dht_key = hex(blake3(owner_pubkey_bytes)[..16])
```

This is a one-way derivation: observers of the DHT see only an opaque hex string.
Sidecars compute the same key from `owner_public_key_b64` in their `SidecarMetadata`
and perform a Kademlia provider lookup for it.

A proxy serving multiple tenants announces itself under multiple derived keys,
one per tenant whose cert it holds.

The global `podmesh-proxy-node` key MUST NOT be used for workload proxy discovery.
Sidecars discover proxies only through the tenant-derived key.

### Proxy NodeCert (tenant-signed)

The proxy holds a `NodeCert` issued by the tenant (signed by the owner's Ed25519 key).
The cert is provisioned out-of-band by the operator using podctl and loaded by the proxy
at startup (or reloaded without restart). It is NOT delivered via `SidecarMetadata`.

```
NodeCert {
    peer_id: String,           // proxy's libp2p PeerId
    kem_pubkey: String,        // base64 X25519 — proxy's KEM key
    signing_pubkey: String,    // base64 Ed25519 — proxy's own signing key
    capabilities: ["proxy"],   // capability marker
    role: NodeRole::Proxy,
    valid_until: u64,          // unix timestamp secs (configurable TTL)
    owner_pubkey: String,      // base64 Ed25519 — tenant's owner key (issuer)
    owner_sig: String,         // Ed25519 sig over canonical_bytes(NodeCert)
    endorsements: [],
}
```

The proxy presents this cert during connection establishment — specifically, by including
`proxy_cert_b64` (base64 postcard NodeCert) as an extension field in the existing
`/podmesh/handshake/1.0.0` response payload.

### Sidecar Verification (on connection)

After completing the handshake with a proxy candidate, the sidecar:

1. Extracts `proxy_cert_b64` from the handshake response.
2. Deserializes the `NodeCert`.
3. Verifies:
   - `NodeCert.verify()` — `owner_sig` is a valid Ed25519 sig by `owner_pubkey` over
     `canonical_bytes(NodeCert)`
   - `NodeCert.owner_pubkey == SidecarMetadata.owner_public_key_b64` — same tenant
   - `!NodeCert.is_expired()` — `valid_until > now`
   - Handshake response signer peer_id == `NodeCert.peer_id` — cert matches the peer
4. If any check fails: close the connection, do not send `SidecarRegistration`.
5. If all checks pass: proceed with `SidecarRegistration` to this proxy.

The sidecar MUST NOT fall back to the unauthenticated `podmesh-proxy-node` key if
no tenant-authenticated proxy is found.

### Proxy Verification of Sidecar (on SidecarRegistration)

The proxy verifies:

1. `SidecarRegistration.sig` is a valid Ed25519 sig over `manifest_id || sidecar_peer_id`
   using `SidecarRegistration.sidecar_signing_pubkey`.
2. `SidecarRegistration.owner_pubkey` matches the `owner_pubkey` in the proxy's `NodeCert`
   for this tenant — the sidecar's workload belongs to the same tenant that issued the cert.
3. The transport peer_id of the connection == `SidecarRegistration.sidecar_peer_id`.

`SidecarRegistration` is extended with `sidecar_signing_pubkey` (the sidecar's own Ed25519
public key) to make verification self-contained without relying on prior state.

## Actors

- **Tenant (podctl operator)** — signs a `NodeCert` for the proxy; provisions it to the proxy
- **podmesh-proxy** — holds the tenant-signed cert; announces under obfuscated tenant DHT key;
  presents cert in handshake; verifies sidecar registration owner
- **podmesh-sidecar** — discovers proxy via derived DHT key; verifies cert on connect
- **podmesh-agent** — derives `SidecarMetadata` from the verified encrypted deployment grant;
  `owner_public_key_b64` is already present

## Data Structure Changes

### NodeRole (shared/protocol)

```rust
pub enum NodeRole {
  Proxy,
}
```

### SidecarRegistration (shared/protocol)

```rust
pub struct SidecarRegistration {
    pub manifest_id: String,
    pub routes: Vec<SidecarRoute>,
    pub sidecar_peer_id: String,
    pub owner_pubkey: String,           // unchanged
    pub sig: String,                    // unchanged: sig over manifest_id || sidecar_peer_id
    pub sidecar_signing_pubkey: String, // sidecar's own Ed25519 pubkey
}
```

### Handshake response (shared/p2p, machine protocol)

The existing handshake response (built in `handshake.rs:build_signed_handshake_response`)
is extended to optionally carry `proxy_cert_b64: Option<String>` in the signed envelope.
Non-proxy nodes leave this field absent. The sidecar checks for its presence after handshake.

### SidecarMetadata — no change

`SidecarMetadata` is not changed. The sidecar already receives `owner_public_key_b64`
which is sufficient to derive the DHT lookup key and to verify the proxy cert.

## Trust Flow

```
podctl (offline or pre-deploy)
  │  signs NodeCert { peer_id=proxy, signing_pubkey=proxy_ed25519_pk,
  │                   owner_pubkey=owner_pk, role=Proxy, valid_until=T }
  │  provisions cert to proxy (out of band: file, flag, or API)
  ▼
podmesh-proxy starts
  │  loads NodeCert from disk / config
  │  computes: proxy_dht_key = hex(blake3(owner_pubkey_bytes)[..16])
  │  announces as Kademlia provider for proxy_dht_key
  │  (may announce for multiple tenants if holding multiple certs)
  ▼
podmesh-sidecar starts
  │  reads owner_public_key_b64 from SidecarMetadata
  │  computes: proxy_dht_key = hex(blake3(owner_pubkey_bytes)[..16])
  │  Kademlia provider lookup for proxy_dht_key → candidate peer(s)
  │  dials candidate, performs /podmesh/handshake/1.0.0
  ▼
Handshake exchange:
  sidecar ──handshake request──► proxy
  proxy ◄──handshake response (includes proxy_cert_b64)──
  sidecar extracts and verifies NodeCert:
    1. NodeCert.verify() — owner_sig valid
    2. NodeCert.owner_pubkey == owner_public_key_b64
    3. !NodeCert.is_expired()
    4. handshake signer peer_id == NodeCert.peer_id
  if fail → close connection
  ▼
SidecarRegistration (sidecar → proxy):
  SidecarRegistration {
      manifest_id, routes, sidecar_peer_id,
      owner_pubkey,           // tenant's pubkey
      sidecar_signing_pubkey, // sidecar's own Ed25519 pubkey
      sig,                    // Ed25519(sidecar_sk, manifest_id || sidecar_peer_id)
  }
  proxy verifies:
    1. sig valid under sidecar_signing_pubkey
    2. owner_pubkey == NodeCert.owner_pubkey (same tenant)
    3. transport peer_id == sidecar_peer_id
  if fail → reject, do not store route
```

## Trust Properties After This Change

| Property | How it holds |
|---|---|
| Sidecar only connects to tenant's proxy | DHT lookup key is derived from owner pubkey; proxy cert is verified on connect |
| DHT does not leak owner identity | DHT key is `blake3(owner_pubkey)[..16]` — one-way |
| Proxy cert forgery requires owner private key | cert is signed with Ed25519 owner_sk; no owner_sk → can't forge |
| Proxy cannot serve cross-tenant sidecars | proxy rejects `SidecarRegistration` where `owner_pubkey` ≠ cert issuer |
| Sidecar identity is bound to transport | proxy checks transport peer_id == `sidecar_peer_id` in registration |
| Rogue proxy in DHT is rejected | sidecar verifies cert on handshake; rogue proxy has no valid tenant-signed cert |

## Remaining Open Questions

- `podctl cert grant-proxy` currently POSTs the cert to the proxy REST API.
  Should file-based provisioning at startup also be supported for offline/bootstrap workflows?
- Should the proxy re-announce the derived DHT key periodically (like sidecar provider records)?
  What is the TTL?
- If a proxy holds certs for multiple tenants, does it announce under all derived keys simultaneously?
- Should the sidecar retry with a different proxy candidate if the first one fails cert verification?

## Non-Goals

- CA hierarchy — the owner keypair is the sole trust anchor.
- Revoking a proxy cert before expiry — not addressed in this change.
- Encrypting the cert separately in transit — libp2p QUIC/TLS already encrypts the transport.
- Protecting the owner pubkey against a peer who has already established a connection —
  obfuscation applies to passive DHT observers only.
