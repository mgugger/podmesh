## Purpose

Defines idempotent, revision-bound updates of existing Podmesh deployments while preserving replica
placement, owner authority, partial-progress recovery, and mandatory service-mesh participation.

## ADDED Requirements

### Requirement: Applying an existing deployment SHALL update it

`podctl apply` SHALL derive the stable deployment id from the owner and workload name. If that
deployment already exists, the command SHALL update its replicas rather than create another
deployment or overwrite the catalog with unrelated placements.

#### Scenario: Unchanged deployment is idempotent

- **GIVEN** every replica already runs the requested revision
- **WHEN** the owner applies the same manifest again
- **THEN** no replica is redeployed
- **AND** the existing placements and catalog remain unchanged

#### Scenario: Changed manifest updates existing replicas

- **GIVEN** a deployment whose requested revision differs from the running revision
- **WHEN** the owner applies the changed manifest
- **THEN** each existing replica is updated on its currently selected agent
- **AND** no additional replica or workload id is created

### Requirement: MVP updates SHALL preserve the replica count

An update SHALL require the requested replica count to equal the deployment's recorded replica
count. Scaling is not part of the MVP update operation.

#### Scenario: Replica count change is refused

- **GIVEN** an existing three-replica deployment
- **WHEN** the owner applies the same workload name with a different replica count
- **THEN** `podctl` refuses the update before changing any replica
- **AND** it reports that scaling is not supported in the MVP

### Requirement: Updates SHALL use revision compare-and-swap

Every update request SHALL bind the workload id, target agent, expected current revision, requested
revision, owner identity, issue time, expiry, and nonce. The agent SHALL update the workload only
when the stored revision equals the expected revision or already equals the requested revision.

#### Scenario: Stale update is refused

- **GIVEN** a workload has advanced from revision A to revision B
- **WHEN** an update expecting revision A attempts to install revision C
- **THEN** the agent refuses it without changing the running workload

#### Scenario: Replayed completed update is idempotent

- **GIVEN** the workload already runs the requested revision
- **WHEN** the same logical update is retried with a fresh valid request
- **THEN** the agent returns a receipt for the running revision without replacing it again

### Requirement: Partial update progress SHALL remain retryable

`podctl` SHALL write the catalog atomically after each replica reaches the requested revision. A
failed update SHALL retain each replica's last confirmed revision and placement.

#### Scenario: One replica fails during update

- **GIVEN** three replicas and an update that succeeds on two agents
- **WHEN** the third agent does not complete the update
- **THEN** the command reports the per-replica failure
- **AND** the catalog records the new revision for the two successful replicas
- **AND** a retry updates only replicas not already at the requested revision

### Requirement: Updates SHALL preserve mandatory service-mesh configuration

Every updated execution specification SHALL carry current proxy records, relay credentials, workload
credentials, routes, and sidecar configuration. An update SHALL NOT replace a service-mesh workload
with an unmeshed workload.

#### Scenario: Updated workload remains reachable

- **WHEN** an application image or configuration is updated
- **THEN** the replacement replica includes the injected sidecar
- **AND** it registers the same owner-derived routing key after startup

