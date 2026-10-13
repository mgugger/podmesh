## MODIFIED Requirements

### Requirement: A sidecar SHALL prove its tenancy rather than assert it

A sidecar SHALL present owner-rooted authorization binding its tenant, workload, replica incarnation, authenticated Iroh EndpointId and application signing key. Verifiers SHALL check issuer scope, validity and transport identity before serving workload traffic. Bearer workload credentials SHALL NOT authorize sessions.

The root authority SHALL be delivered inside owner-encrypted execution configuration. Scoped delegated enrollment SHALL bind the sidecar-generated public keys without conveying the owner private key. Components carrying stored authority SHALL verify structure and binding without treating expired stored configuration as a reason to delete a workload. Live authorization SHALL enforce validity.

Operations SHALL use the tenancy proven by the connection, never repeated caller assertions. Tenancy SHALL NOT outlive the authenticated and authorized session.

#### Scenario: A long-running workload survives an agent restart
- **GIVEN** a workload with expired child credentials
- **WHEN** its agent reconciles after restart
- **THEN** expired child credentials do not cause workload deletion
- **AND** traffic remains refused until valid endpoint authorization is available

#### Scenario: Naming a tenant is not enough to act as it
- **WHEN** a caller presents only a public owner key or unbound bearer token
- **THEN** registration, discovery and workload tunnels are refused

#### Scenario: A credential does not carry to another workload
- **WHEN** a credential is presented for another routing identity
- **THEN** the operation is refused

#### Scenario: A copied credential does not authorize another endpoint
- **WHEN** a peer presents a credential naming a different EndpointId
- **THEN** it is refused even if its signatures and validity window are correct

### Requirement: Ingress SHALL be balanced across every replica

Every replica SHALL register the same owner-derived routing key, and the proxy SHALL rotate between eligible replica backends. Backends SHALL be keyed by replica index with authorized incarnation handling, not accumulated merely because an endpoint changes. Unreachable replicas SHALL be retried independently; the routing key SHALL disappear only when its last backend is gone.

#### Scenario: Every replica serves traffic
- **GIVEN** several eligible replicas register a deployment
- **WHEN** requests arrive for it
- **THEN** every replica serves some requests

#### Scenario: A restarted replica replaces itself
- **WHEN** a replica restarts with its persistent identity or an explicitly authorized new incarnation
- **THEN** it updates/replaces its own backend instead of creating a duplicate

#### Scenario: An unreachable replica does not blackhole the request
- **WHEN** a selected replica cannot be reached
- **THEN** the proxy tries another eligible replica

#### Scenario: One dead replica does not remove its siblings
- **WHEN** one replica stops refreshing registration
- **THEN** that backend is pruned while siblings keep serving

### Requirement: Sidecars SHALL register routes with the proxy

The sidecar SHALL announce ingress routes to the proxy over authenticated streams. Internal service discovery SHALL instead permit bounded tenant-scoped peer synchronization independent of proxy route tables. There SHALL be no DHT or Kademlia in the workload plane.

Route selection SHALL prefer a host-scoped route over a host-agnostic one, then the longest matching path prefix, and SHALL be deterministic among equally specific matches.

#### Scenario: Ingress after registration
- **GIVEN** a sidecar registered for a host and path
- **WHEN** matching external traffic arrives
- **THEN** the proxy forwards to the sidecar and its authorized local application

#### Scenario: Discovery requires a live grant
- **WHEN** a proxy discovery request arrives for a tenant with no live proxy grant
- **THEN** the proxy refuses it

#### Scenario: Internal discovery does not require a proxy route
- **WHEN** authorized sidecars synchronize internal services
- **THEN** records are verified without consulting a proxy route table

### Requirement: Egress SHALL be tunnelled through the proxy and bounded

The sidecar SHALL forward non-internal application TCP traffic through an authorized, bounded proxy egress tunnel. Synthetic internal destinations SHALL instead use authorized sidecar-to-sidecar routing and SHALL NOT fall back to proxy egress.

Proxy tunnels SHALL require proven tenancy and a live proxy grant for that tenant. Non-internal destinations SHALL NOT be filtered by address; a granted tenant can reach services, private addresses and metadata accessible to that proxy. The proxy SHALL resolve each destination once and dial only returned addresses.

Tunnels SHALL remain byte/time bounded. Refusals SHALL NOT distinguish forbidden from unreachable destinations.

#### Scenario: Application reaches an external destination
- **WHEN** an application opens an intercepted connection to a non-internal destination
- **THEN** it traverses the authorized proxy egress path

#### Scenario: A tunnel requires a proven and granted tenant
- **WHEN** an unproven or ungranted tenant requests proxy egress
- **THEN** the request is refused

#### Scenario: A refusal does not identify the reason
- **WHEN** proxy egress is refused
- **THEN** the caller receives the same bounded refusal regardless of its cause

#### Scenario: An unknown internal destination never becomes public egress
- **WHEN** a reserved synthetic address has no authorized service mapping
- **THEN** the connection fails without proxy forwarding

## ADDED Requirements

### Requirement: Workloads SHALL have local split DNS
The sidecar SHALL answer DNS inside the workload namespace over UDP and TCP. It SHALL resolve internal suffixes only from authorized tenant discovery state and forward other names to the preserved runtime/host resolver. Internal failures SHALL NOT reach upstream DNS.

#### Scenario: Internal service resolves locally
- **WHEN** an authorized internal service has eligible replicas
- **THEN** an A query returns its local synthetic service address

#### Scenario: Internal discovery is unavailable
- **WHEN** an internal service cannot be resolved from available authorized state
- **THEN** DNS returns SERVFAIL unless authoritative evidence permits another response
- **AND** no public DNS request is made

#### Scenario: Public and runtime names retain upstream behavior
- **WHEN** a non-internal name is queried
- **THEN** the original configured upstream is used with bounded UDP/TCP handling

### Requirement: DNS service addresses SHALL remain safe under replica churn
Synthetic service addresses SHALL represent service identities rather than hosts. Changes to replicas SHALL NOT change an existing local service address. Stale addresses SHALL NOT be reassigned to another service in a way that misroutes cached client answers.

#### Scenario: A replica disappears
- **WHEN** a service loses one replica
- **THEN** its existing synthetic address still routes new connections to eligible survivors

#### Scenario: Internal IPv6 is not supported
- **WHEN** AAAA is queried for a known IPv4-only internal service
- **THEN** the response contains no fabricated unreachable IPv6 address

### Requirement: Peer service records SHALL be authorized and bounded
Peers SHALL admit only owner-rooted, endpoint-bound service records matching authorized service names and ports. Records SHALL expire no later than their credentials and merge per replica incarnation/sequence. Peer counts, state, frames, query fanout and synchronization work SHALL be bounded.

#### Scenario: A peer claims another service or signing key
- **WHEN** a record exceeds its credential scope or uses an unauthorized signing key
- **THEN** the record is refused without changing existing service state

#### Scenario: An expired replica cannot remain eligible
- **WHEN** a record or its credential expires
- **THEN** that replica is excluded from new connections

#### Scenario: Reconnection repairs a cache
- **WHEN** peers reconnect with different verified record sets
- **THEN** bounded synchronization exchanges missing/newer valid records

### Requirement: Internal TCP SHALL authenticate both workloads
Direct TCP setup SHALL authenticate both endpoint-bound peers, enforce tenant and service-port authority, and forward only to an authorized local application target. It SHALL retain raw-transfer limits and authority rechecks. Replica retry SHALL occur only before application bytes are forwarded.

#### Scenario: Direct service traffic bypasses proxies
- **WHEN** an application connects to an internal service with a reachable remote replica
- **THEN** its sidecar carries the connection to that replica over authenticated Iroh without a proxy session

#### Scenario: Peer requests an arbitrary application target
- **WHEN** a peer names a host or port outside the authorized service mapping
- **THEN** tunnel setup is refused

#### Scenario: A selected replica is unavailable
- **WHEN** setup fails before forwarding application bytes
- **THEN** another eligible replica can be selected without globally revoking the failed one

### Requirement: DNS and synthetic routing SHALL not rely on UID equality for isolation
Interception SHALL capture application DNS and synthetic service traffic even when the application and sidecar share a UID. Sidecar forwarding SHALL bypass its own interception without resolver loops. DNS SHALL listen only within the workload namespace and bound requests, responses and handler concurrency.

#### Scenario: Application shares the sidecar UID
- **WHEN** that application queries DNS or connects to a service VIP
- **THEN** it reaches local DNS/internal routing rather than bypassing those paths

#### Scenario: Sidecar forwards a non-internal query
- **WHEN** its upstream socket sends the query
- **THEN** that packet is not redirected back into its DNS listener
