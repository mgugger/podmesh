## 1. Shared Identity And Protocol

- [x] 1.1 Add bounded proxy peer and proxy-discovery request/response wire types with validation
  tests in `shared/protocol`.
- [x] 1.2 Add secure persistent libp2p identity loading and first-run initialization in
  `shared/p2p`, with restart, permissions, missing-key, and malformed-key tests.
- [x] 1.3 Add the proxy-discovery protocol ID and remove shared Kademlia constants and helpers.

## 2. Proxy Runtime

- [x] 2.1 Add proxy key-directory and configured regional peer settings, then build the swarm with
  the persisted identity.
- [x] 2.2 Add bounded in-memory proxy peer tracking and the proxy-discovery request-response handler.
- [x] 2.3 Remove the proxy Kademlia behaviour, readiness/provider announcements, tenant DHT
  notifications, manifest lookup fallback, and related state.
- [x] 2.4 Add proxy tests for identity stability, bounded peer responses, malformed requests, and
  fail-closed routing without a sidecar registration.

## 3. Sidecar And Agent Runtime

- [x] 3.1 Replace `SidecarMetadata.bootstrap_peer` with bounded explicit proxy peer records and add
  metadata validation tests.
- [x] 3.2 Update the encrypted execution specification and sidecar injection to provide several
  tenant-owned address-bearing proxy peer records.
- [x] 3.3 Remove the sidecar Kademlia behaviour, provider publication, lookups, and manifest records;
  dial configured proxy records directly.
- [x] 3.4 Fetch additional peers from verified proxies, validate returned records, handshake every
  candidate, and use only tenant-certified proxies for registration and egress.
- [x] 3.5 Add sidecar tests for multiple regional bootstrap peers, mismatched peer IDs, stale
  candidates, cross-tenant rejection, and successful peer exchange.

## 4. Integration And Documentation

- [x] 4.1 Update deployment configuration, support helpers, and integration tests for persistent
  proxy keys and explicit proxy peer records.
- [x] 4.2 Update architecture documentation to describe direct registration, durable regional proxy
  identities, peer exchange, and removal of Kademlia.
- [x] 4.3 Run formatting, crate-focused tests, and the complete workspace test suite.