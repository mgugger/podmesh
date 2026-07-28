# podctl-cli Specification

## Purpose

`podctl` is the namespace owner's command line tool. It is a plain HTTP client with no Iroh
endpoint: it reaches any scheduler over HTTP and the scheduler relays control traffic to agents.
It owns the tenant Ed25519/X25519 keys, decides replica placement, and encrypts every workload
payload so that no intermediary can read or forge it.

## Requirements

### Requirement: podctl SHALL NOT operate an Iroh endpoint

`podctl` SHALL communicate exclusively over HTTP with a scheduler chosen by the operator. It SHALL
NOT bind an Iroh endpoint, join gossip, or dial agents directly.

#### Scenario: Applying a manifest against a reachable scheduler

- **WHEN** the operator runs `podctl --api-url <scheduler> apply -f <manifest>`
- **THEN** every request is an HTTP request to `<scheduler>`
- **AND** no Iroh endpoint is created in the `podctl` process

#### Scenario: Any scheduler is interchangeable

- **GIVEN** three schedulers in the mesh
- **WHEN** the operator points `--api-url` at any one of them
- **THEN** the command succeeds with identical results, because schedulers are stateless

### Requirement: podctl SHALL decide replica placement

`podctl` SHALL read `spec.replicas` or the `podmesh.io/replicas` annotation, pin the manifest it
ships to a single pod, and request one agent per replica. Each request SHALL exclude the agents
already selected. The scheduler SHALL NOT learn the replica count and SHALL NOT fan a deployment
out on its own.

#### Scenario: Three replicas land on three distinct agents

- **GIVEN** a manifest with `spec.replicas: 3` and at least three agents with capacity
- **WHEN** the operator applies it
- **THEN** `podctl` issues three `GET /api/v1/agents/select` calls
- **AND** each call after the first passes `?exclude=` listing the previously selected agents
- **AND** `podctl` admits and deploys against each returned agent itself

#### Scenario: Fewer agents than replicas

- **GIVEN** a manifest requesting more replicas than there are eligible agents
- **WHEN** a selection request cannot be satisfied
- **THEN** `podctl` reports the shortfall and the already-deployed replicas remain running

### Requirement: podctl SHALL own the tenant keys and encrypt every payload

`podctl` SHALL load or create the owner Ed25519 signing keypair and X25519 KEM keypair under
`~/.podmesh/` with `0600` permissions. Every admission request, deployment grant, and lifecycle
command SHALL be signed by the owner signing key and encrypted to the selected agent's KEM key.

#### Scenario: Scheduler cannot read a relayed payload

- **WHEN** `podctl` posts an encrypted deployment grant to the scheduler
- **THEN** the scheduler relays opaque bytes to the agent
- **AND** the scheduler can neither decrypt nor re-sign the payload

#### Scenario: Lifecycle commands require the owner key

- **GIVEN** a workload deployed by one owner key
- **WHEN** a different key issues a status, logs, or delete command
- **THEN** the agent rejects it

### Requirement: podctl SHALL mint owner-signed proxy grants

Before deploying, `podctl` SHALL mint a bounded, expiring Biscuit grant for each configured proxy
and POST it to that proxy's `POST /api/v1/proxy_grant`. The grant SHALL bind the tenant owner
public key, the proxy endpoint identifier, an issue time, an expiry, and a unique token id. Its
lifetime SHALL NOT exceed `MAX_PROXY_GRANT_LIFETIME_SECS`.

#### Scenario: Proxy presents the grant to a sidecar

- **WHEN** a proxy handshakes with a tenant's sidecar
- **THEN** it presents the grant minted by that tenant's owner key
- **AND** the sidecar verifies the signature, the tenant owner, the proxy endpoint, and the expiry

#### Scenario: Grant for a different proxy is rejected

- **WHEN** a proxy presents a grant whose proxy endpoint does not match its own endpoint id
- **THEN** the sidecar refuses the connection

### Requirement: podctl SHALL bootstrap proxy configuration over HTTP

When `PODMESH_PROXY_URL` is set, `podctl` SHALL fetch each proxy's signed `EndpointRecord`, the
workload relay auth token, and the relay CA certificate from that proxy's REST API. All listed
proxies SHALL agree on the relay token; disagreement SHALL be a hard error. Explicitly supplied
values SHALL take precedence over bootstrapped ones.

#### Scenario: Proxies disagree on the relay token

- **GIVEN** two proxies that minted independent relay tokens
- **WHEN** both are listed in `PODMESH_PROXY_URL`
- **THEN** `podctl` aborts rather than shipping a sidecar that can reach only one relay

#### Scenario: Bootstrapped records are verified

- **WHEN** `podctl` receives a signed `EndpointRecord` over plain HTTP
- **THEN** it verifies the signature and expiry before using it
- **AND** a tampered record is discarded
