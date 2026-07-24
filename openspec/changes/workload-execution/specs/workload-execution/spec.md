# Workload Execution

## ADDED Requirements

### Requirement: Workload execution is encrypted end to end

The complete execution specification MUST be encrypted between the namespace client and selected
agent. The scheduler and unrelated agents MUST NOT receive plaintext or the DEK.

#### Scenario: Selected agent receives a workload
- **WHEN** podctl deploys an admitted workload
- **THEN** only the selected agent can unwrap the DEK and decrypt the complete specification

### Requirement: Agent validates admission and deployment independently

The selected agent MUST verify owner identity, request expiry, replay nonce, aggregate resources,
reservation binding, target node, workload identity, revision identity, and grant signature before
execution.

#### Scenario: Agent validates a deployment
- **WHEN** the agent receives an encrypted grant
- **THEN** it verifies owner, expiry, replay nonce, resources, reservation, target, workload identity, revision, and signature before execution

### Requirement: Agent isolates workload lifecycle state

Each workload MUST have an independently encrypted persistent record and independently authorized
status, logs, and delete commands. Deleting one workload MUST NOT modify another.

#### Scenario: One workload is deleted
- **WHEN** an owner deletes one of several workloads on an agent
- **THEN** only that workload runtime and encrypted record are removed

### Requirement: Scheduler is not in the lifecycle path

Running workloads, restart reconciliation, status, logs, and deletion MUST remain functional without
scheduler persistence.

#### Scenario: Scheduler is unavailable
- **WHEN** an owner requests status, logs, or deletion using a receipt
- **THEN** the operation is handled directly by the agent without scheduler state