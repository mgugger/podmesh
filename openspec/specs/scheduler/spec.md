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

### Requirement: Bounded fanout SHALL NOT make an agent permanently unplaceable

A capacity query is sent to a bounded number of attached agents, because a scheduler holding
thousands of them must not open a stream to every one on every placement. Which agents that bound
selects SHALL vary per query, so that no agent is systematically excluded.

Selecting by a fixed order — such as sorting by `EndpointId` — truncates to the same agents every
time, so every agent past the bound is never asked for capacity and can never be placed on. That
failure is silent: the fleet looks healthy and the capacity simply never appears.

The ordering SHALL be derived from the agent together with the query, so that it is uniform across
agents, uncorrelated between queries, and reproducible for one query. It SHALL NOT require the
scheduler to keep state, which a rotating cursor would.

#### Scenario: Every agent is eventually asked

- **GIVEN** more attached agents than the fanout bound
- **WHEN** many placements are made
- **THEN** every attached agent is reached by some query

#### Scenario: One query is reproducible

- **WHEN** the same query is fanned out twice
- **THEN** it reaches the same agents, so a "no capacity" answer means the same thing on retry

### Requirement: The scheduler SHALL NOT be trusted to choose the recipient

A capacity offer is self-signed: the key that validates it is carried inside it, and the offer also
names the KEM key every payload is sealed to. Verifying an offer therefore proves only that it is
internally consistent. Whoever answers a selection request chooses the recipient, and so chooses who
can read the workload.

The scheduler SHALL NOT be relied on to constrain that choice. The owner SHALL decide which agent
signing keys it is willing to deploy to, and SHALL check every offer against that decision before
producing any ciphertext.

#### Scenario: A scheduler cannot nominate itself as the agent

- **WHEN** a scheduler answers a selection request with an offer it signed itself
- **THEN** the client refuses it, because that key is not one the owner trusts

### Requirement: The scheduler SHALL select agents from signed capacity offers

On `GET /api/v1/agents/select` the scheduler SHALL gossip a bounded, signed, short-lived
`CapacityQuery` and collect signed `CapacityOffer`s. It SHALL return one offer. The `exclude`
query parameter SHALL withhold the listed agent endpoint ids from consideration.

A selection SHALL answer as soon as it has collected as many offers as the caller
asked to choose between, rather than always waiting for the query to expire. A
client places replicas one at a time, so a selection that always ran to expiry
would make a deployment cost that timeout once per replica regardless of how fast
the mesh answered. A query serving several callers SHALL keep the greediest
caller's target, so joining one never shortens a wait somebody else asked for.

#### Scenario: Excluded agents are never returned

- **WHEN** a client passes `?exclude=<hex>,<hex>`
- **THEN** the returned offer is from an agent not in the exclude list

#### Scenario: Selection answers as soon as the mesh has

- **WHEN** the offers a caller asked to choose between have arrived
- **THEN** the selection answers immediately rather than waiting out the query
  lifetime

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

### Requirement: Locating an agent SHALL cost the same in a large mesh as in a small one

A scheduler SHALL find the holder of an attachment by broadcasting one signed location query on the
gossip mesh, which only the holder answers, and SHALL NOT connect to peers one by one to ask. Asking
each peer costs one connection per peer for every client request, so in a mesh of thousands of
schedulers the search — not the work — would dominate.

The query SHALL carry no payload, SHALL be signed by the asking scheduler, and SHALL name a reply
endpoint belonging to that same signer, so an answer cannot be steered at a third party. A query
SHALL expire, and a receiver SHALL accept it only from a scheduler in its member allowlist.

The answer SHALL be delivered by the holder over the control relay protocol, so the asker
authenticates the answering scheduler exactly as it authenticates relayed traffic, and the holder
SHALL be taken from that authenticated connection rather than from anything the message claims. An
answer to a query the scheduler did not ask SHALL be ignored, so no member can plant locations.

Learned locations SHALL be cached with a bounded size and lifetime, and a location that fails to
deliver SHALL be forgotten, so one stale entry cannot fail every later request for that agent.

#### Scenario: Locating an agent does not move the payload

- **WHEN** a scheduler searches for the peer holding an attachment
- **THEN** the query it broadcasts carries no payload
- **AND** it transfers the payload only to the peer that answered

#### Scenario: One unreachable peer does not slow every relayed request

- **GIVEN** a mesh in which one peer does not answer
- **WHEN** a scheduler locates an agent held by a peer that does answer
- **THEN** it returns as soon as that peer answers rather than waiting for the rest

#### Scenario: Repeated operations on one agent do not re-query the mesh

- **WHEN** a client sends several operations to the same agent through one scheduler
- **THEN** the location is resolved once and served from cache afterwards

#### Scenario: A stale location does not become permanent

- **GIVEN** a cached location that no longer holds the agent
- **WHEN** a client addresses that agent
- **THEN** the scheduler forgets the entry and resolves the location again

#### Scenario: An unsolicited location answer is ignored

- **WHEN** a member sends an answer for a query this scheduler never issued
- **THEN** nothing is cached and no caller is woken

### Requirement: The scheduler SHALL broadcast an owner's list request

On `POST /api/v1/workloads/list` the scheduler SHALL relay an owner-signed list
request to every agent attached to it, bounded in fan-out and concurrency, and
return each agent's sealed answer unopened together with the agents that did not
answer.

The request is not sealed and names no agent, because a client that has lost its
index cannot name the agents to ask. The scheduler SHALL still learn nothing: it
holds no tenant key, and each answer is sealed to the requesting owner.

#### Scenario: A client learns which agents did not answer

- **WHEN** some agents do not respond to a list broadcast
- **THEN** they are reported as unreachable, so a partial view is not mistaken
  for a complete one

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

### Requirement: Membership SHALL grow beyond the peers configured on each scheduler

A cluster of thousands of schedulers cannot require every scheduler to be listed in every other
scheduler's configuration, so each scheduler SHALL periodically announce its own signed endpoint
record on the gossip mesh, and SHALL admit into its member allowlist any scheduler whose announcement
it receives. Announcements SHALL be repeated, so a scheduler that joins later still learns the ones
already present, and SHALL expire, so a stale address is not kept forever.

Admission this way is transitive but not unbounded: only a scheduler already admitted by somebody can
reach the gossip mesh at all, so an announcement is always vouched for by an operator-configured
member. The member allowlist SHALL remain bounded.

Announcing SHALL NOT grant unrestricted relay-issuer trust. That stays with peers pinned through
HTTP discovery, because an unrestricted issuer can mint a grant naming any subject and so could hand
a relay's bandwidth to arbitrary third parties.

An announcement SHALL instead bind the announcing scheduler's signing key to its endpoint id, and a
machine relay SHALL honour a grant from such a key only when the grant's subject is that same
endpoint. Since the relay separately requires the subject to be the endpoint that authenticated the
connection, an announced member can authorise only itself.

This is what lets relay reachability scale with membership. Without it a scheduler that is only ever
admitted by announcement, and that cannot be reached directly, would have no relay willing to carry
it, because pinning is bounded by the handful of peer URLs an operator writes down while membership
reaches thousands.

The binding SHALL NOT be treated as proof of anything else. An endpoint record is self-signed, so
anyone may announce any endpoint id alongside their own key; the binding is safe only because the
subject must match the authenticated connection, and is therefore useless to anyone who does not
already control that endpoint.

#### Scenario: An announced scheduler reaches a peer's relay

- **GIVEN** two schedulers that know each other only through announcements and cannot connect directly
- **WHEN** one uses the other's machine relay with a grant it signed for itself
- **THEN** the relay admits it, without either having been pinned

#### Scenario: An announced scheduler cannot admit a third party

- **WHEN** an announced member signs a relay grant naming another endpoint as subject
- **THEN** the relay refuses it

#### Scenario: An agent behind NAT needs no converged issuer trust

- **WHEN** an agent attaches to a scheduler and uses that scheduler's relay
- **THEN** the grant it presents was minted by that same scheduler, which its relay already trusts

#### Scenario: Two schedulers that share only a hub learn each other

- **GIVEN** two schedulers configured to know only a common third scheduler
- **WHEN** they announce themselves on the mesh
- **THEN** each admits the other and can relay control traffic through it

#### Scenario: An announcement does not confer relay-issuer trust

- **WHEN** a scheduler is admitted through an announcement rather than through a pinned peer URL
- **THEN** it is not added to the relay trusted issuer set

### Requirement: Each peer URL SHALL be bound to one scheduler identity

A discovered peer is admitted to the gossip allowlist *and* trusted to issue relay grants, so
substituting one is a takeover of the control plane. Because a peer's `EndpointRecord` is self-signed
— whoever answers the URL picks the key that validates it — a signature alone SHALL NOT be treated as
authority.

Each peer URL SHALL therefore be bound to one endpoint id and one signing key. An operator MAY
configure the binding up front; otherwise the first successful observation SHALL be persisted. A
later observation that contradicts the binding SHALL be refused and logged, never silently accepted.

#### Scenario: A substituted peer identity is refused

- **GIVEN** a scheduler that has already discovered a peer at a URL
- **WHEN** that URL later answers with a different endpoint id or signing key
- **THEN** the scheduler refuses it and keeps the identity it pinned

#### Scenario: An operator can remove the first-observation window

- **WHEN** a peer's identity is configured out of band
- **THEN** that binding is used from the first sweep and a remembered one does not override it

#### Scenario: Registries cannot be filled by a hostile responder

- **WHEN** one peer URL answers with a new random identity on every sweep
- **THEN** only the pinned identity is ever admitted, so the member and issuer registries cannot be
  exhausted

### Requirement: The scheduler client API SHALL be unauthenticated for the first release

The mesh is open: any keypair may ask any scheduler to place a workload, and the scheduler relays
owner-encrypted bytes it cannot read. Authentication of the client API is therefore deliberately
absent for this release, and the resulting exposure SHALL be stated rather than implied away.

An unauthenticated caller can enumerate the agent fleet through selection, occupy placement capacity,
and cause fan-out across the mesh. Concurrency, body size, probe count, and pending-query counts SHALL
each be bounded, and every client-facing route SHALL be rate limited per peer address, so that this
stays a nuisance rather than an outage. The API SHALL NOT be described as protected: a rate limit
bounds how fast anyone may call, not who may.

Liveness routes SHALL stay outside the limiter, so an orchestrator probing health is never throttled
by unrelated client traffic. The limit SHALL key on the peer address of the connection and never on a
forwarded-for header, which the caller chooses.

#### Scenario: Every relay path is concurrency bounded

- **WHEN** many control requests arrive at once, whether for locally attached agents or for agents
  held by a peer
- **THEN** each path acquires a permit and excess requests are refused rather than queued without
  bound

#### Scenario: A single caller cannot fan out without limit

- **WHEN** one peer address exceeds its request budget on selection or on a control relay route
- **THEN** further requests are refused with 429 rather than being served

#### Scenario: Liveness answers even under throttling

- **WHEN** a caller has exhausted its budget
- **THEN** the health and readiness routes still answer

### Requirement: The scheduler SHALL self-provision its relay credentials

The scheduler SHALL generate and persist its machine relay TLS keypair and trust its own signing key
as an issuer when no credentials are configured. No operator-created secret SHALL be required to
start a scheduler.

#### Scenario: Cold start with an empty key directory

- **WHEN** a scheduler starts with no existing credentials
- **THEN** it generates them, writes them with restrictive permissions, and serves its relay
