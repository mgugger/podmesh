## ADDED Requirements

### Requirement: The agent SHALL enforce one runtime pod per replica

The agent SHALL independently reject any execution specification that can cause the runtime to
create more than one pod for the workload. Client-side replica normalization SHALL NOT be trusted as
an enforcement boundary.

#### Scenario: Custom client sends several runtime replicas

- **WHEN** a signed manifest requests more than one runtime replica
- **THEN** the agent refuses it before reservation consumption or runtime execution

### Requirement: The agent SHALL allow only supported manifest documents

For the MVP, an execution specification SHALL contain exactly one supported pod-bearing document and
only explicitly supported service-mesh routing documents. Persistent volumes, secrets, controllers
with independent scaling semantics, and unknown kinds SHALL be refused before runtime execution.

#### Scenario: Standalone persistent volume claim is included

- **WHEN** a manifest contains a persistent volume claim document
- **THEN** the agent refuses the complete workload
- **AND** the runtime creates no volume or pod

#### Scenario: More than one pod-bearing document is included

- **WHEN** a manifest contains two documents capable of creating pods
- **THEN** the agent refuses it rather than accounting for only one

### Requirement: Resource accounting SHALL cover every executable container

Every regular, init, or ephemeral container that the runtime can execute SHALL either receive
bounded defaults and be included in measured resources, or cause the workload to be refused.

#### Scenario: Ephemeral container cannot escape accounting

- **WHEN** a manifest declares an ephemeral container
- **THEN** its limits are included in the reservation or the manifest is refused

### Requirement: Runtime operations SHALL have bounded concurrency

Deploy, update, status, logs, delete, and restart reconciliation operations that invoke the
container runtime SHALL use explicit time and concurrency bounds.

#### Scenario: Runtime operation limit is saturated

- **WHEN** more runtime operations arrive than the configured concurrency bound
- **THEN** excess operations are refused or wait only in a bounded queue
- **AND** the agent does not spawn unbounded runtime processes

### Requirement: One unreadable persisted workload SHALL NOT stop other workloads

The agent SHALL load and validate persisted records independently. A record that cannot be
decrypted, decoded, or matched to its key SHALL be isolated and reported without preventing valid
records from being reconciled.

#### Scenario: One encrypted row is corrupt

- **GIVEN** several valid workload records and one corrupt record
- **WHEN** the agent restarts
- **THEN** valid workloads remain manageable
- **AND** only the corrupt record is quarantined or removed

### Requirement: Agent state SHALL be private from creation

The workload-state directory and database file SHALL be created with owner-only permissions rather
than created broadly and narrowed afterward.

#### Scenario: State is created on a permissive umask

- **WHEN** the agent creates a new workload store
- **THEN** the directory is never observable with permissions wider than `0700`
- **AND** the file is never observable with permissions wider than `0600`

