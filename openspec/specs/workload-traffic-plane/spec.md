# Workload Traffic Plane Specification

## Purpose

The proxy and sidecar form the workload traffic plane. The proxy is the ingress and egress gateway;
the sidecar is the in-pod companion that registers routes and forwards traffic to the application
container. They authenticate each other with owner-signed Biscuit grants over the
`/podmesh/workload/1` Iroh protocol.

## Requirements

### Requirement: The proxy SHALL prove the tenant authorised it

The proxy SHALL present a Biscuit grant minted by the tenant owner during the workload handshake.
The sidecar SHALL verify the grant against the tenant owner public key it was injected with, the
connecting proxy's endpoint id as taken from the authenticated transport, and the current time.

This is one half of a mutual exchange. The other half — the sidecar proving which tenant it belongs
to — is a separate requirement below, because each side proves something the other cannot infer.

#### Scenario: Handshake without a grant is refused

- **WHEN** a proxy handshakes without an owner-signed grant
- **THEN** the sidecar closes the connection

#### Scenario: Expired grant is refused

- **WHEN** a presented grant's expiry has passed, allowing for bounded clock skew
- **THEN** the sidecar closes the connection

#### Scenario: Grant from a foreign owner is refused

- **WHEN** the grant's tenant owner does not match the sidecar's injected owner key
- **THEN** the sidecar closes the connection

#### Scenario: A grant cannot be replayed by a different proxy

- **WHEN** a proxy presents a grant naming an endpoint other than its own
- **THEN** the sidecar closes the connection, because the expected endpoint comes from the
  authenticated transport rather than from the grant

### Requirement: A sidecar SHALL NOT run without a tenant owner key

The owner key is the only thing a sidecar can check a proxy's grant against. A sidecar injected
without one SHALL refuse to start, rather than run in a state where any reachable proxy could reach
the application.

#### Scenario: A sidecar without an owner key refuses to start

- **WHEN** sidecar metadata carries no tenant owner key
- **THEN** the sidecar exits with an error

### Requirement: Biscuit grants SHALL NOT be used for external ingress

Grants SHALL govern only the proxy-to-sidecar relationship. External clients reaching the proxy's
ingress listener SHALL NOT be required to present a grant.

#### Scenario: External HTTP request reaches ingress

- **WHEN** an external client sends an HTTP request to the proxy ingress port
- **THEN** the proxy routes it to a registered sidecar without demanding a Biscuit from the client

### Requirement: The proxy REST API SHALL be rate limited per peer

`POST /api/v1/proxy_grant` is unauthenticated by design — a grant proves its own authority — but
verifying one costs a signature check and a Datalog evaluation, and every accepted grant occupies a
slot in a bounded store. Every REST route except liveness SHALL therefore be rate limited per peer
address.

#### Scenario: Submitting grants past the budget is refused

- **WHEN** one peer address exceeds its request budget on the grant endpoint
- **THEN** further submissions are refused with 429 rather than verified

#### Scenario: Liveness answers even under throttling

- **WHEN** a caller has exhausted its budget
- **THEN** the health route still answers

### Requirement: The proxy SHALL bound its grant store

The proxy SHALL store at most `MAX_TENANT_GRANTS` tenant grants, SHALL re-verify a grant before
each use, and SHALL evict expired grants. Grant storage SHALL NOT grow without bound.

#### Scenario: Store at capacity

- **WHEN** the store already holds the maximum number of grants
- **THEN** accepting a new grant does not grow the store beyond that bound

#### Scenario: Expired grant is not served

- **WHEN** a stored grant has expired
- **THEN** it is evicted and no handshake presents it

### Requirement: A sidecar SHALL prove its tenancy rather than assert it

A namespace owner's public key is public, and a sidecar's transport key is generated inside the
container, so neither identifies the tenant a connection belongs to. A sidecar SHALL therefore
present an owner-signed workload credential during the proxy handshake, naming the tenant owner and
the routing key it serves, and the proxy SHALL record that tenancy against the connection only after
verifying it. Because the owner key is also the credential's signing root, a credential cannot be
minted for a tenant whose private key the minter does not hold.

The credential SHALL be minted by `podctl` at deploy time, SHALL travel inside the owner-encrypted
execution specification, and SHALL expire. It is a bearer credential: whoever reads a pod's metadata
may act as that workload, which SHALL be stated rather than implied away. Binding it to the sidecar's
transport key is not possible, because that key does not exist when the owner mints it.

Expiry SHALL be enforced where the authorisation decision is made — at the proxy — and SHALL NOT be
enforced by components that merely carry the credential. A pod may run for longer than its
credential's lifetime, and an agent re-reads a stored execution specification on every restart;
refusing there would delete a healthy workload because of an unrelated restart. Those components
SHALL still verify the signature and the tenant and workload it names.

#### Scenario: A long-running workload survives an agent restart

- **GIVEN** a workload still running after its credential expired
- **WHEN** its agent restarts and reconciles
- **THEN** the workload is restored, and the proxy still refuses the expired credential

Operations on a connection SHALL be checked against the proven tenancy, never against anything the
caller repeats. Tenancy SHALL NOT outlive the connection that proved it.

#### Scenario: Naming a tenant is not enough to act as it

- **GIVEN** a proxy holding a live grant from one tenant
- **WHEN** a caller that does not hold that tenant's private key presents its public key
- **THEN** the proxy refuses the connection's registration, discovery and egress

#### Scenario: A credential does not carry to another workload

- **WHEN** a credential minted for one workload is presented for another routing key
- **THEN** the proxy refuses it

### Requirement: A routing key SHALL belong to exactly one tenant

The routing key a sidecar registers under SHALL be derived from the tenant owner public key and the
workload name, and the proxy SHALL recompute it. A registration SHALL be accepted only when its
endpoint matches the authenticated transport, its routing key matches the owner key it names, the
connection has proven that owner and that routing key, and the proxy already holds a live grant
signed by that owner.

The proven tenancy is what makes the rest meaningful: the derivation uses only public inputs, so
without it a caller could recompute another tenant's routing key and claim it.

#### Scenario: A registration cannot claim another tenant's routing key

- **WHEN** a registration names a routing key that is not derived from the owner key it presents
- **THEN** the proxy refuses it

#### Scenario: A registration must match what the connection proved

- **WHEN** a registration names an owner or routing key other than the one proven at handshake
- **THEN** the proxy refuses it

#### Scenario: An existing entry is never silently reassigned

- **GIVEN** routes registered by one owner
- **WHEN** a different owner registers the same routing key
- **THEN** the proxy refuses it and the original routes keep serving

### Requirement: Hostnames SHALL be claimed, and claims SHALL NOT be transferable

A hostname SHALL serve a workload only if that workload claimed it, or if the hostname is the
canonical `<routing-key>.mesh.local` form. An unrecognised `Host` header SHALL NOT be treated as a
routing key. A hostname already claimed by one owner SHALL NOT be reassigned to another, and a
rejected registration SHALL leave no partial claim behind.

#### Scenario: An arbitrary Host header does not address a workload

- **WHEN** a client sends a `Host` header no workload has claimed
- **THEN** the proxy answers that no workload serves that host

#### Scenario: A hostname cannot be taken over

- **GIVEN** a hostname claimed by one tenant
- **WHEN** another tenant registers routes claiming the same hostname
- **THEN** the registration is refused and the first tenant keeps the hostname

### Requirement: Ingress SHALL be balanced across every replica

Every replica of a deployment registers the same routing key, because they serve
the same workload. The proxy SHALL hold one backend per replica and rotate
between them, so consecutive requests reach different replicas.

Backends SHALL be keyed by replica index rather than by the sidecar's transport
identity: a restarted replica returns with a fresh, ephemeral endpoint id and
would otherwise appear beside the dead one until that timed out.

When a replica cannot be reached the proxy SHALL try another, so a replica that
died since its last registration refresh does not blackhole traffic. Pruning
SHALL be per replica, and a routing key SHALL disappear only when its last
replica is gone.

#### Scenario: Every replica serves traffic

- **GIVEN** a deployment with several replicas registered
- **WHEN** a series of requests arrives for it
- **THEN** every replica serves some of them

#### Scenario: A restarted replica replaces itself

- **WHEN** a replica restarts and registers with a new transport identity
- **THEN** it replaces its own backend rather than appearing alongside it

#### Scenario: An unreachable replica does not blackhole the request

- **WHEN** the replica selected for a request cannot be reached
- **THEN** the proxy tries another replica

#### Scenario: One dead replica does not remove its siblings

- **WHEN** one replica stops refreshing its registration
- **THEN** it is pruned while its siblings keep serving

### Requirement: Sidecars SHALL register routes with the proxy

The sidecar SHALL announce its routes to the proxy over an authenticated stream, and the proxy SHALL
resolve sidecar `EndpointRecord`s per workload. There SHALL be no DHT, Kademlia, or gossip in the
workload plane.

Route selection SHALL prefer a host-scoped route over a host-agnostic one, then the longest matching
path prefix, and SHALL be deterministic among equally specific matches.

#### Scenario: Ingress after registration

- **GIVEN** a sidecar that has registered a host and path
- **WHEN** a matching external request arrives at the proxy
- **THEN** the proxy forwards it to that sidecar, which proxies it to the application on localhost

#### Scenario: Discovery requires a live grant

- **WHEN** a discovery request arrives for a tenant with no live grant held by the proxy
- **THEN** the proxy refuses it

### Requirement: Egress SHALL be tunnelled through the proxy and bounded

The sidecar SHALL forward application-originated traffic through an egress tunnel to the proxy,
which SHALL relay it to the destination.

A tunnel SHALL be authorised by tenant: the connection SHALL have proven a tenant owner, and the
proxy SHALL hold a live grant from that owner. Destinations SHALL NOT be filtered by address.
Address filtering was removed deliberately — podmesh runs across machines whose application parts
legitimately live on private networks, so filtering blocks real traffic while a granted tenant's
reach is unchanged. The consequence SHALL be stated: a tenant that granted a proxy can reach
whatever that proxy can reach, including its own services and cloud instance metadata.

The proxy SHALL resolve the destination once and dial only the addresses that resolution returned,
so a name cannot resolve to one host for a check and another for the connection.

A tunnel SHALL be bounded in bytes and in time, so it cannot hold a stream slot indefinitely. A
refusal SHALL NOT distinguish an unreachable destination from a forbidden one, so the tunnel is not
usable as a port scanner.

#### Scenario: Application reaches an external destination

- **WHEN** the application container opens a connection to an external host
- **THEN** it traverses the sidecar egress tunnel and the proxy rather than leaving the pod directly

#### Scenario: A tunnel requires a proven and granted tenant

- **WHEN** a connection that proved no tenant, or whose tenant did not grant this proxy, opens a tunnel
- **THEN** the proxy refuses it

#### Scenario: A refusal does not identify the reason

- **WHEN** an egress destination is refused, whether by policy or because it is unreachable
- **THEN** the answer is the same

### Requirement: Ingress TLS is out of scope for the first release

The ingress listener serves plain HTTP. This SHALL be stated plainly rather than implied away: an
operator terminating TLS must do so in front of the proxy.

#### Scenario: Documentation does not claim transport protection for ingress

- **WHEN** an operator reads the deployment documentation
- **THEN** it states that ingress is unencrypted and that TLS termination is their responsibility

### Requirement: A proxy SHALL be able to advertise an address other than the one it bound

The addresses an endpoint discovers are the ones it bound, which are not reachable when the proxy is
behind NAT, inside a container, or simply on another machine from the sidecars that must dial it.
A proxy SHALL therefore accept an explicit set of addresses to publish in its signed
`EndpointRecord` in place of the discovered ones, and SHALL publish the discovered ones when none is
configured.

Because the record is signed, substituting addresses SHALL NOT let a third party redirect traffic:
the endpoint identity is unchanged and the transport still authenticates it.

#### Scenario: A proxy behind NAT is dialable

- **GIVEN** a proxy whose bound address is not reachable by its sidecars
- **WHEN** it is configured with a routable address
- **THEN** its published record carries that address instead of the bound one

#### Scenario: Nothing configured keeps the discovered addresses

- **WHEN** no address is configured
- **THEN** the record carries the addresses the endpoint discovered

### Requirement: Relay credentials SHALL be scoped to one tenant

A workload's relay credential SHALL be derived from a mesh secret and the tenant it belongs to, and
SHALL carry that tenant in the clear so the relay knows who is connecting before it decides. A
workload SHALL NOT receive the mesh secret. One tenant's token SHALL NOT admit another, so a leaked
pod exposes only its own tenant's relay access.

A proxy MAY be restricted to relay for a named set of tenants, and SHALL relay for any tenant by
default. Relaying is transport only: what a workload may register or tunnel is decided by its
owner-signed credential, so an open relay grants no authority. A restriction SHALL NOT prevent a
proxy from reaching its own relay.

A denial SHALL NOT distinguish an invalid credential from a tenant that is simply not admitted, so
a relay does not disclose which tenants it serves.

#### Scenario: One tenant's relay token does not admit another

- **WHEN** a token derived for one tenant is presented naming a different tenant
- **THEN** the relay refuses the connection

#### Scenario: A restricted relay carries only its listed tenants

- **GIVEN** a proxy configured with a set of relay tenants
- **WHEN** a tenant outside that set presents a valid derived token
- **THEN** the relay refuses it, and the proxy still reaches its own relay

### Requirement: Proxy relay credentials SHALL be self-provisioned and shareable

Each proxy SHALL generate and persist its own relay TLS keypair and mesh secret when none is
configured. Because tokens are derived from that secret, every proxy validating a tenant's token
needs the same secret, so a proxy MAY adopt a peer's over
`GET /api/v1/workload_relay_mesh_secret`. That endpoint discloses a mesh-wide credential, so
publishing it SHALL be opt-in and at most one proxy in a deployment SHALL publish it.

Tenants SHALL use `GET /api/v1/workload_relay_bootstrap`, which returns only the derived token for
the tenant named in the request and SHALL NOT disclose the mesh secret.

#### Scenario: Second proxy adopts the first proxy's mesh secret

- **GIVEN** one proxy started with relay bootstrap publishing enabled
- **WHEN** a second proxy is started with that proxy's bootstrap URL and no explicit secret
- **THEN** it adopts the published secret and both relays admit the same tenant's token

#### Scenario: A tenant asking for credentials does not receive the mesh secret

- **WHEN** a client requests relay bootstrap for its own tenant
- **THEN** it receives only that tenant's derived token

#### Scenario: Explicit secret wins

- **WHEN** both an explicit mesh secret and a bootstrap URL are configured
- **THEN** the explicit secret is used and no peer is contacted

#### Scenario: Peer refuses to publish

- **WHEN** the peer proxy was not started with relay bootstrap publishing enabled
- **THEN** startup fails with an error naming the required flag
