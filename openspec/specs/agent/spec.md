# Agent Specification

## Purpose

The agent is the only component that sees workload plaintext. It admits owner-encrypted requests
within aggregate resource limits, decrypts target-bound grants, injects sidecars, drives Podman,
persists each workload encrypted at rest, and serves lifecycle commands independently per workload.

## Requirements

### Requirement: The agent SHALL admit workloads within aggregate limits

The agent SHALL verify the owner signature and decrypt every admission request addressed to its KEM
key, then reserve CPU, memory, and storage against its configured aggregate capacity. It SHALL
refuse admission beyond `max_workloads`, or beyond its CPU, memory, storage, reservation, payload
size, or replay limits.

#### Scenario: Admission beyond capacity is refused

- **WHEN** an admission request would exceed any configured aggregate limit
- **THEN** the agent refuses it and reserves nothing

#### Scenario: Replayed admission is refused

- **WHEN** an admission request reuses a nonce the agent has already seen
- **THEN** the agent refuses it

#### Scenario: Request encrypted to another agent is unusable

- **WHEN** an agent receives a payload encrypted to a different agent's KEM key
- **THEN** decryption fails and the request is refused

### Requirement: The agent SHALL host many independent workloads

The agent SHALL store one encrypted record per full workload id. Deleting, restarting, or failing
one workload SHALL NOT affect any other workload on the same agent.

#### Scenario: Deleting one workload leaves others running

- **GIVEN** several workloads from different owners on one agent
- **WHEN** one owner deletes their workload
- **THEN** only that workload's containers and record are removed

#### Scenario: Restart reconciles all records

- **WHEN** the agent restarts
- **THEN** it decrypts every persisted record and reconciles each workload's containers locally

### Requirement: The agent SHALL execute workloads through Podman with an injected sidecar

The agent SHALL decrypt the target-bound deployment grant, inject the configured sidecar image with
its metadata, and deploy the pod through Podman. Sidecar metadata SHALL be written to
`/var/run/podmesh/sidecar/metadata.json`.

#### Scenario: Sidecar receives tenant material

- **WHEN** a pod is deployed
- **THEN** its sidecar metadata carries the tenant owner public key, proxy endpoint records, the
  workload relay token, and the relay CA certificates

### Requirement: The agent SHALL serve lifecycle commands only to the owner

Status, logs, and delete commands SHALL be accepted only when signed by the key that owns that
workload and encrypted to the agent. All lifecycle traffic SHALL arrive over Iroh; the agent's HTTP
surface SHALL expose only `GET /health`.

#### Scenario: Non-owner lifecycle command is refused

- **WHEN** a signed command from a key that does not own the workload arrives
- **THEN** the agent refuses it and does not disclose workload existence

#### Scenario: HTTP surface is minimal

- **WHEN** any HTTP path other than `/health` is requested from an agent
- **THEN** the agent does not serve it

### Requirement: The agent SHALL bootstrap scheduler attachments over HTTP

The agent SHALL accept scheduler HTTP URLs, fetch and verify each scheduler's signed
`EndpointRecord`, and merge the results with any explicitly configured endpoints. The number of
bootstrap URLs SHALL be bounded and each response SHALL be size-bounded and time-bounded. A URL
that does not answer SHALL be a startup error.

#### Scenario: Unreachable scheduler URL fails startup loudly

- **WHEN** a configured bootstrap URL cannot be resolved
- **THEN** the agent exits with an error rather than starting half-configured

### Requirement: The agent SHALL answer capacity queries from any scheduler in the mesh

The agent SHALL treat its configured scheduler list as a bootstrap list, not an authorization list,
and SHALL answer a capacity query that originates from a scheduler it never configured. The query
MUST still be signed, unexpired, and unseen, and MUST have arrived over the authenticated
attachment, because the attached scheduler already restricts fan-out to admitted mesh members.

#### Scenario: Placement reaches an agent through a scheduler it does not know

- **GIVEN** an agent attached to exactly one scheduler
- **WHEN** a different scheduler in the mesh originates a capacity query
- **THEN** the agent returns a signed offer directly to that scheduler

#### Scenario: Replayed or expired queries are still refused

- **WHEN** a query is expired, unsigned, or repeats a query id already seen
- **THEN** the agent drops it and stays attached

### Requirement: Remote recovery after agent loss SHALL NOT be implied

Loss of an agent together with its durable keys SHALL be treated as loss of the workloads it hosted.
No component SHALL claim that a single-replica workload survives that loss.

#### Scenario: Single-replica workload on a destroyed agent

- **WHEN** an agent and its key directory are destroyed
- **THEN** its single-replica workloads are gone and are not recovered elsewhere
