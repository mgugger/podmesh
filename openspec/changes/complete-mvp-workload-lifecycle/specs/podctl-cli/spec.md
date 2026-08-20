## ADDED Requirements

### Requirement: podctl SHALL request placement for measured resources

Before selecting an agent, `podctl` SHALL validate the canonical single-pod manifest, include the
injected sidecar defaults, and send the resulting CPU, memory, storage, and required capabilities to
the scheduler. It SHALL NOT solicit placeholder capacity.

#### Scenario: Full agent is not selected for a larger workload

- **GIVEN** one agent with insufficient remaining resources and another agent that fits
- **WHEN** `podctl` places a replica
- **THEN** the selection criteria describe the replica's measured resources
- **AND** the fitting agent can be selected without first failing admission on the full agent

#### Scenario: Capacity changes after selection

- **WHEN** the selected agent loses capacity before admission completes
- **THEN** `podctl` retries another returned or newly selected eligible agent within a bounded limit
- **AND** it excludes agents that already refused that replica

### Requirement: podctl SHALL reconcile its catalog from agent authority

`podctl` SHALL be able to request the owner's workloads through any scheduler, verify and decrypt
agent responses, and atomically rebuild or repair catalog entries. A reconciled placement SHALL
retain the agent identity and cryptographic material needed for later lifecycle commands.

#### Scenario: Deleted catalog is rebuilt

- **GIVEN** replicas still running on agents and no local catalog entry
- **WHEN** the owner runs reconciliation
- **THEN** `podctl` reconstructs the deployment and replica placements from verified agent responses
- **AND** status, logs, update, and delete can use the rebuilt catalog

#### Scenario: Partial discovery is not committed as complete

- **WHEN** one or more schedulers or agents do not answer reconciliation
- **THEN** `podctl` reports the unreachable scope
- **AND** it does not remove locally known replicas merely because they were absent from the partial
  response

### Requirement: podctl SHALL require the workload service mesh in production

Production apply and update operations SHALL require validated proxy endpoint records, relay
credentials, owner-signed proxy grants, and a workload credential for the injected sidecar.

#### Scenario: Production apply has no proxy configuration

- **WHEN** an operator applies a workload without valid service-mesh configuration
- **THEN** `podctl` refuses before admission
- **AND** it explains which proxy bootstrap or explicit configuration is missing

#### Scenario: Test-only bypass is explicit

- **WHEN** an isolated scheduler or agent test disables service-mesh injection
- **THEN** the bypass is explicit and unavailable as the default production behavior

