## ADDED Requirements

### Requirement: Scheduler placement SHALL preserve resource criteria

The scheduler client API SHALL accept bounded CPU, memory, storage, and capability criteria from
`podctl`, bind them into the signed `CapacityQuery`, and select only an offer satisfying those exact
criteria.

#### Scenario: Resource criteria reach agents unchanged

- **WHEN** a client requests placement for a measured replica
- **THEN** the signed query carries those measured requirements
- **AND** an offer below any requirement is discarded

#### Scenario: Malformed resource criteria are refused

- **WHEN** a caller supplies missing, zero, overflowing, or out-of-bound criteria
- **THEN** the scheduler refuses the request without gossiping a query

### Requirement: Owner reconciliation SHALL be mesh-wide and ephemeral

On an owner reconciliation request, the reached scheduler SHALL coordinate a bounded request across
the scheduler mesh. Each scheduler SHALL ask only its locally attached agents, and all responses
SHALL remain sealed to the owner. Pending coordination state SHALL expire and SHALL NOT become a
durable workload index.

#### Scenario: Reconciliation through a scheduler with no agents

- **GIVEN** the reached scheduler holds no agent attachments
- **WHEN** the owner requests reconciliation
- **THEN** agents attached to peer schedulers can still answer
- **AND** the reached scheduler returns their sealed responses

#### Scenario: Scheduler restart retains no reconciliation state

- **WHEN** a scheduler restarts after a reconciliation request
- **THEN** it has no durable workload records from that request
- **AND** a later reconciliation obtains fresh answers from agents

#### Scenario: Duplicate agent answers are deduplicated

- **GIVEN** an agent is reachable through more than one attachment path
- **WHEN** its answer reaches the coordinator more than once
- **THEN** the result contains one answer for that agent endpoint

