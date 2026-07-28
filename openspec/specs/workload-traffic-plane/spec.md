# Workload Traffic Plane Specification

## Purpose

The proxy and sidecar form the workload traffic plane. The proxy is the ingress and egress gateway;
the sidecar is the in-pod companion that registers routes and forwards traffic to the application
container. They authenticate each other with owner-signed Biscuit grants over the
`/podmesh/workload/1` Iroh protocol.

## Requirements

### Requirement: Proxy and sidecar SHALL authenticate with owner-signed Biscuit grants

The proxy SHALL present a Biscuit grant minted by the tenant owner during the workload handshake.
The sidecar SHALL verify the grant against the tenant owner public key it was injected with, the
connecting proxy's endpoint id, and the current time. Biscuits are used so that a grant can later be
attenuated and delegated without reissuing.

#### Scenario: Handshake without a grant is refused

- **WHEN** a proxy handshakes without an owner-signed grant
- **THEN** the sidecar closes the connection

#### Scenario: Expired grant is refused

- **WHEN** a presented grant's expiry has passed, allowing for bounded clock skew
- **THEN** the sidecar closes the connection

#### Scenario: Grant from a foreign owner is refused

- **WHEN** the grant's tenant owner does not match the sidecar's injected owner key
- **THEN** the sidecar closes the connection

### Requirement: Biscuit grants SHALL NOT be used for external ingress

Grants SHALL govern only the proxy-to-sidecar relationship. External clients reaching the proxy's
ingress listener SHALL NOT be required to present a grant.

#### Scenario: External HTTP request reaches ingress

- **WHEN** an external client sends an HTTP request to the proxy ingress port
- **THEN** the proxy routes it to a registered sidecar without demanding a Biscuit from the client

### Requirement: The proxy SHALL bound its grant store

The proxy SHALL store at most `MAX_TENANT_GRANTS` tenant grants, SHALL re-verify a grant before
each use, and SHALL evict expired grants. Grant storage SHALL NOT grow without bound.

#### Scenario: Store at capacity

- **WHEN** the store already holds the maximum number of grants
- **THEN** accepting a new grant does not grow the store beyond that bound

#### Scenario: Expired grant is not served

- **WHEN** a stored grant has expired
- **THEN** it is evicted and no handshake presents it

### Requirement: Sidecars SHALL register routes with the proxy

The sidecar SHALL announce its routes to the proxy over an authenticated stream, and the proxy SHALL
resolve sidecar `EndpointRecord`s per workload. There SHALL be no DHT, Kademlia, or gossip in the
workload plane.

#### Scenario: Ingress after registration

- **GIVEN** a sidecar that has registered a host and path
- **WHEN** a matching external request arrives at the proxy
- **THEN** the proxy forwards it to that sidecar, which proxies it to the application on localhost

#### Scenario: Discovery requires a live grant

- **WHEN** a discovery request arrives for a tenant with no live grant held by the proxy
- **THEN** the proxy refuses it

### Requirement: Egress SHALL be tunnelled through the proxy

The sidecar SHALL forward application-originated traffic through an egress tunnel to the proxy,
which SHALL relay it to the destination.

#### Scenario: Application reaches an external destination

- **WHEN** the application container opens a connection to an external host
- **THEN** it traverses the sidecar egress tunnel and the proxy rather than leaving the pod directly

### Requirement: Proxy relay credentials SHALL be self-provisioned and shareable

Each proxy SHALL generate and persist its own relay TLS keypair and auth token when none is
configured. Because a sidecar is injected with exactly one relay token, a proxy MAY adopt a peer
proxy's token over `GET /api/v1/workload_relay_bootstrap`. Publishing that endpoint SHALL be
opt-in, since it discloses a live token.

#### Scenario: Second proxy adopts the first proxy's token

- **GIVEN** one proxy started with relay bootstrap publishing enabled
- **WHEN** a second proxy is started with that proxy's bootstrap URL and no explicit token
- **THEN** it adopts the published token and a sidecar's single token reaches both relays

#### Scenario: Explicit token wins

- **WHEN** both an explicit token and a bootstrap URL are configured
- **THEN** the explicit token is used and no peer is contacted

#### Scenario: Peer refuses to publish

- **WHEN** the peer proxy was not started with relay bootstrap publishing enabled
- **THEN** startup fails with an error naming the required flag
