## ADDED Requirements

### Requirement: Proxy identity is durable
Each logical proxy MUST load its libp2p identity from protected persistent storage and MUST retain
the same peer ID across ordinary process, container, host, and address changes.

#### Scenario: Proxy restarts with existing identity
- **WHEN** a proxy restarts with its existing valid identity key file
- **THEN** it exposes the same libp2p peer ID as before the restart

#### Scenario: Initialized identity is unavailable
- **WHEN** an initialized proxy cannot securely load its configured identity key
- **THEN** startup fails without generating a replacement identity

### Requirement: Sidecar receives explicit proxy records
The tenant MUST include a bounded non-empty list of proxy peer records in the encrypted,
owner-signed execution specification. The agent MUST validate and inject that list into sidecar
metadata, and each record MUST bind a peer ID to at least one bounded dialable multiaddr ending in
that peer ID.

#### Scenario: Sidecar starts from multiple regions
- **WHEN** sidecar metadata contains valid proxy records for multiple regions
- **THEN** the sidecar attempts the records without using DNS or Kademlia discovery

#### Scenario: Address identity mismatch
- **WHEN** a proxy record address contains a `/p2p` component different from the record peer ID
- **THEN** the sidecar rejects that record before dialing it

### Requirement: Proxy candidates are tenant authenticated
The sidecar MUST treat configured and discovered proxy records as untrusted candidates and MUST NOT
use a proxy for registration, ingress routing, or egress until it verifies a valid tenant-issued
`NodeCert` bound to the transport peer and role `Proxy`.

#### Scenario: Valid regional proxy
- **WHEN** a candidate presents a valid non-expired `NodeCert` issued by the workload tenant and
  bound to its transport peer ID
- **THEN** the sidecar may register routes and use that proxy for workload traffic

#### Scenario: Cross-tenant candidate
- **WHEN** a candidate presents a valid certificate issued by a different tenant
- **THEN** the sidecar rejects the candidate and does not use it for workload traffic

### Requirement: Sidecar can fetch additional proxies
A verified proxy MUST expose a bounded proxy-discovery request-response protocol through which a
sidecar can request additional proxy records for its tenant.

#### Scenario: Additional regional proxies are available
- **WHEN** a verified sidecar requests more proxies with a valid tenant owner key and result limit
- **THEN** the proxy returns at most the allowed number of bounded peer records without workload
  data

#### Scenario: Returned candidate is stale
- **WHEN** a returned peer record cannot be dialed or fails tenant certificate verification
- **THEN** the sidecar discards it and continues using other valid proxies

### Requirement: Discovery state is bounded soft state
Proxy peer records, pending discovery requests, and sidecar routing registrations MUST be bounded
in count and size and MUST be reconstructible after process restart without durable membership
state.

#### Scenario: Peer response exceeds bounds
- **WHEN** a peer sends a discovery request or response exceeding configured protocol bounds
- **THEN** the receiver rejects it without allocating unbounded memory or changing eligible proxies

### Requirement: Workload routing uses direct registration
A proxy MUST route workload traffic only through a current authenticated sidecar registration and
MUST NOT perform a Kademlia or manifest-provider lookup when no registration exists.

#### Scenario: Registered workload
- **WHEN** a sidecar has authenticated and registered a workload route with a proxy
- **THEN** the proxy routes matching workload traffic to that sidecar

#### Scenario: Workload is not registered
- **WHEN** no current authenticated registration exists for a workload
- **THEN** the proxy returns service unavailable without a distributed lookup

### Requirement: Runtime discovery is Kademlia free
Proxy and sidecar runtime behaviours MUST NOT initialize Kademlia, publish provider or mutable
records, or perform provider or record lookups.

#### Scenario: Proxy and sidecar start
- **WHEN** a proxy or sidecar starts
- **THEN** no Kademlia routing table, bootstrap query, provider announcement, or record publication
  is created