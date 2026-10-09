## ADDED Requirements

### Requirement: Uncertain deployment outcomes SHALL retain ownership and accounting

Before runtime creation can have side effects, the agent SHALL persist a target-bound pending
record and account for its resources. Runtime failure, timeout, response loss, cancellation or a
failed final persistence step SHALL NOT silently release accounting or erase the only recoverable
record. A deployment SHALL become active only after runtime and durable-state confirmation.

Cleanup SHALL target only that workload's agent-derived runtime identity, preserve the original
failure and use finite operation bounds. State and accounting SHALL be removed only after runtime
cleanup is confirmed and the record is durably removed. An uncertain remote-runtime outcome SHALL
remain explicitly repairable; an unconfirmed timeout SHALL NOT be treated as proof of absence.

#### Scenario: Runtime creation partially succeeds before failure

- **WHEN** deployment creates runtime state and then fails
- **THEN** the agent attempts targeted bounded cleanup and retains charged state until removal is confirmed
- **AND** no successful deployment receipt is returned

#### Scenario: Cleanup or final persistence fails

- **WHEN** cleanup cannot complete or durable record removal fails
- **THEN** an actionable owner-bound record and its resource charge remain for retry or restart
- **AND** the original operation error is not replaced by cleanup diagnostics

#### Scenario: The outer request is cancelled

- **WHEN** dispatch is cancelled after runtime creation may have started
- **THEN** the prewritten pending state survives without relying on asynchronous drop cleanup
- **AND** no successful deletion or accounting release is inferred from cancellation

#### Scenario: The runtime result remains uncertain

- **WHEN** local command cancellation does not establish whether remote Podman work has completed
- **THEN** the agent retains the repair-required record and charge rather than assuming the pod is absent

### Requirement: Pending workload mutations SHALL be serialized per workload

The agent SHALL prevent overlapping create, update and delete side effects for one workload while
allowing unrelated workloads to operate. A pending or cleanup-required workload SHALL remain
discoverable by its owner and report its non-active state. Startup or explicit owner cleanup SHALL
resolve pending failures conservatively instead of blindly redeploying an uncommitted create.

#### Scenario: Delete arrives during creation

- **WHEN** an owner requests deletion while that workload's runtime create remains in flight
- **THEN** the operation is serialized or refused as busy without reporting successful deletion
- **AND** resource accounting remains held until the target outcome is resolved

#### Scenario: Cleanup is retried after restart

- **WHEN** the agent restarts with a pending-create or cleanup-required record
- **THEN** it retains ownership and accounting while resolving that record
- **AND** it does not recreate an uncommitted failed deployment as a healthy active workload

## MODIFIED Requirements

### Requirement: The agent SHALL host many independent workloads

The agent SHALL store one encrypted record per full workload id. Deleting, restarting, or failing
one workload SHALL NOT affect another workload's runtime or ownership. Uncertain retained resource
usage may conservatively reduce available capacity; this SHALL NOT stop existing healthy workloads.

#### Scenario: Deleting one workload leaves others running

- **GIVEN** several workloads from different owners on one agent
- **WHEN** one owner deletes their workload
- **THEN** only that workload's containers and record are removed

#### Scenario: Restart reconciles all records

- **WHEN** the agent restarts
- **THEN** it attempts to decrypt every persisted record and resolves each readable workload locally

#### Scenario: One unreadable record does not stop the agent

- **GIVEN** a persisted record that cannot be read or safely reconciled
- **WHEN** the agent restarts
- **THEN** it retains/quarantines that record instead of assuming its resources are free
- **AND** healthy sibling workloads remain available while admissions conservatively reflect uncertain usage

#### Scenario: Restart succeeds long after deployment

- **GIVEN** a workload deployed more than a proxy record's lifetime ago
- **WHEN** the agent restarts and reconciles it
- **THEN** reconciliation succeeds, because a stored record's structure and signature are checked
  but its freshness is not