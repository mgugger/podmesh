# Scheduler Specification

## Purpose

The scheduler is a stateless selector over signed, expiring agent advertisements and a blind relay
for owner-encrypted control traffic. It holds no workload state, no keys belonging to tenants, and
no durable agent records.

## Requirements

### Requirement: The scheduler SHALL remain stateless

The scheduler SHALL NOT access Podman, store workload ciphertext, hold tenant keys, track lifecycle
state, retain status or logs, perform deletion, inject sidecars, or persist durable agent records.
Restarting a scheduler SHALL NOT lose any information the mesh depends on.

#### Scenario: Scheduler restart is transparent

- **GIVEN** workloads running on agents
- **WHEN** every scheduler is restarted
- **THEN** the workloads keep running
- **AND** subsequent lifecycle commands succeed once agents have re-attached

#### Scenario: Scheduler cannot forge owner traffic

- **WHEN** a scheduler relays an admission, deployment, or lifecycle payload
- **THEN** it forwards the owner-encrypted bytes unchanged
- **AND** any modification causes the agent to reject the payload

### Requirement: The scheduler SHALL select agents from signed capacity offers

On `GET /api/v1/agents/select` the scheduler SHALL gossip a bounded, signed, short-lived
`CapacityQuery` and collect signed `CapacityOffer`s. It SHALL return one offer. The `exclude`
query parameter SHALL withhold the listed agent endpoint ids from consideration.

#### Scenario: Excluded agents are never returned

- **WHEN** a client passes `?exclude=<hex>,<hex>`
- **THEN** the returned offer is from an agent not in the exclude list

#### Scenario: Unsigned or expired offers are discarded

- **WHEN** an offer fails signature verification, is expired, or replays a seen nonce
- **THEN** it is discarded and does not influence selection

#### Scenario: A malformed gossiped query does not stop the scheduler

- **WHEN** a peer gossips a query that fails validation
- **THEN** the scheduler logs and drops it
- **AND** the capacity coordinator keeps serving subsequent requests

### Requirement: The scheduler SHALL relay control traffic to a named agent

The scheduler SHALL accept persistent authenticated Iroh attachments from agents over
`/podmesh/agent-capacity/1` and relay owner-encrypted payloads over `/podmesh/agent-control/1` for
`POST /api/v1/agents/{endpoint_id}/{admission,deploy,command}`. The `{endpoint_id}` SHALL be the
agent's Iroh endpoint id as lowercase hex.

#### Scenario: Relay to an agent no scheduler holds

- **WHEN** a client addresses an agent that is not attached anywhere in the mesh
- **THEN** the scheduler returns an error rather than buffering the payload

### Requirement: Any scheduler SHALL be a valid control entry point

A scheduler that does not hold the target attachment SHALL relay the owner-encrypted payload
through the peer scheduler that does, over `/podmesh/agent-control-relay/1`, because an agent holds
exactly one attachment while a client may address any scheduler. The peer hop SHALL be taken at
most once: a scheduler serving a relayed request SHALL consult only its own attachments and SHALL
NOT forward it onward. The relayed bytes SHALL be the client's bytes unchanged, so the extra hop
grants no scheduler any ability it did not already have.

#### Scenario: Client reaches an agent attached to another scheduler

- **GIVEN** an agent attached to scheduler B
- **WHEN** a client sends an admission, deployment, or lifecycle payload to scheduler A
- **THEN** scheduler A relays it through scheduler B and returns the agent's answer

#### Scenario: A relayed request never fans out

- **WHEN** a scheduler receives a relayed control request for an agent it does not hold
- **THEN** it answers that it does not hold the agent
- **AND** it does not ask any further peer

#### Scenario: Only admitted schedulers may relay

- **WHEN** an endpoint outside the member allowlist opens the control relay protocol
- **THEN** the connection is refused

#### Scenario: Locating an agent does not move the payload

- **WHEN** a scheduler searches for the peer holding an attachment
- **THEN** it probes peers without the payload
- **AND** it transfers the payload only to the peer that claims the attachment

### Requirement: The scheduler SHALL publish its identity over HTTP for bootstrap

The scheduler SHALL serve a self-signed, self-expiring `EndpointRecord` at
`GET /api/v1/endpoint_record` together with its endpoint id and signing public key. Serving this
over plain HTTP SHALL NOT weaken trust, because consumers verify the signature and expiry.

#### Scenario: Agent bootstraps without hand-copied configuration

- **GIVEN** an agent configured only with scheduler HTTP URLs
- **WHEN** it starts
- **THEN** it fetches each scheduler's `EndpointRecord`, verifies it, and attaches over Iroh

#### Scenario: Tampered record is rejected

- **WHEN** a record's signature does not match the advertised signing public key
- **THEN** the consumer discards it

### Requirement: Scheduler membership SHALL converge without ordering constraints

Each scheduler SHALL accept a list of peer HTTP URLs, including its own, and poll them in the
background. Discovered peers SHALL be added to the gossip member allowlist and the relay trusted
issuer set at runtime, bounded by `MAX_CONVERGED_MEMBERS` and `MAX_CONVERGED_ISSUERS`. A peer that
is not yet reachable SHALL NOT be an error.

#### Scenario: Schedulers start in any order

- **GIVEN** three schedulers each listing all three peer URLs
- **WHEN** they are started simultaneously or in any order
- **THEN** each eventually admits the other two and joins the gossip mesh

#### Scenario: Own endpoint is skipped

- **WHEN** a scheduler discovers a record matching its own endpoint id
- **THEN** it does not add itself as a peer

### Requirement: The scheduler SHALL self-provision its relay credentials

The scheduler SHALL generate and persist its machine relay TLS keypair and trust its own signing key
as an issuer when no credentials are configured. No operator-created secret SHALL be required to
start a scheduler.

#### Scenario: Cold start with an empty key directory

- **WHEN** a scheduler starts with no existing credentials
- **THEN** it generates them, writes them with restrictive permissions, and serves its relay
