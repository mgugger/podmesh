# podmesh-agent

## ADDED Requirements

### Requirement: Agent validates encrypted workload admission

The agent MUST decrypt and verify owner signature, namespace, workload ID, resource bounds, expiry,
and nonce before issuing a short-lived reservation.

#### Scenario: Capacity is available
- **WHEN** the agent receives a valid encrypted admission request within aggregate limits
- **THEN** it returns an agent-signed reservation bound to namespace, workload, resources, and expiry

### Requirement: Agent accepts only target-bound grants

The agent MUST reject grants for another node, invalid or consumed reservations, wrong workload
identity, tampered ciphertext, replayed nonce, or expired authorization.

#### Scenario: Grant targets another node
- **WHEN** a validly signed grant names a different agent node ID
- **THEN** the agent rejects it without decrypting or starting a runtime

### Requirement: Agent persists encrypted local state

The agent MUST persist deployment intent before runtime creation and MUST encrypt that record to its
persistent node KEM key.

#### Scenario: Agent restarts
- **WHEN** the agent opens its persistent database after process restart
- **THEN** it decrypts and reconciles every independent workload record

### Requirement: Agent owns local workload lifecycle

Only owner-signed encrypted commands MAY read status/logs or delete the workload. The scheduler MUST
NOT participate in these operations.

#### Scenario: Owner deletes a workload
- **WHEN** the agent verifies an encrypted owner-signed delete command
- **THEN** it removes only the addressed runtime and record

### Requirement: Agent runs multiple bounded workloads

An agent MUST admit independent workloads while aggregate CPU, memory, storage, reservation count,
and configured workload count remain within bounds. Deleting one workload MUST NOT affect another.
Automatic remote recovery and replica handoff are not required.

#### Scenario: Several workloads fit capacity
- **WHEN** independent workload admissions remain within count, CPU, memory, storage, and reservation limits
- **THEN** the agent accepts and executes them concurrently

### Requirement: Signed reservations bound actual runtime limits

The agent MUST measure the post-sidecar, policy-mutated manifest and reject deployment or restart if
aggregate CPU, memory, or ephemeral storage limits exceed the owner-bound signed reservation.

#### Scenario: Manifest understates its admission request
- **WHEN** post-sidecar measured limits exceed the signed reservation
- **THEN** the agent rejects deployment or restart before runtime execution