# Agent Specification

## Purpose

The agent is the only component that sees workload plaintext. It admits owner-encrypted requests
within aggregate resource limits, enforces the pod security policy, decrypts target-bound grants,
injects sidecars, drives Podman, persists each workload encrypted at rest, and serves lifecycle
commands independently per workload.

The mesh is open: any keypair may ask any agent to run a workload. Authority is therefore not about
*who* may deploy, but about what a deployment may do and who may touch it afterwards.

## Requirements

### Requirement: The agent SHALL expose bounded application metrics only when configured

The agent SHALL accept optional `--metrics-listen` / `PODMESH_METRICS_LISTEN` configuration and
serve it on a dedicated unauthenticated listener. Missing configuration SHALL mean no socket.
Explicit bind or collision failure SHALL fail startup before the machine plane begins.

Metrics SHALL cover scheduler attachment, admission, reservation, deploy, update, status, logs,
delete, reconciliation, runtime restore, and metrics scrape operations. State gauges SHALL report
absolute active reservation, running workload, and attached scheduler counts from authoritative
agent state. Replay, store, and fanout saturation events SHALL use fixed aggregate labels only.

#### Scenario: Restart reconstructs agent gauges

- **WHEN** an agent restores persisted workloads
- **THEN** counters and histograms begin empty
- **AND** running-workload and reservation gauges reflect the reconstructed current state

#### Scenario: Metrics cannot change workload state

- **WHEN** metric recording fails during admission, deployment, update, or deletion
- **THEN** the operation retains the same authorization and persistence result

### Requirement: Sidecar metrics configuration SHALL remain outside signed workload metadata

The agent MAY accept `--sidecar-metrics-listen` /
`PODMESH_AGENT_SIDECAR_METRICS_LISTEN`. When set, injection SHALL add the same address to the sidecar
as `PODMESH_METRICS_LISTEN` and SHALL declare its TCP container port. The value is non-secret
deployment configuration and SHALL NOT be added to `PODMESH_SIDECAR_METADATA_B64`.

#### Scenario: Injected sidecar metrics are explicitly enabled

- **WHEN** the agent sidecar metrics setting is `0.0.0.0:9230`
- **THEN** the injected sidecar receives `PODMESH_METRICS_LISTEN=0.0.0.0:9230`
- **AND** its container declares TCP port 9230

#### Scenario: Omitted propagation remains disabled

- **WHEN** the agent has no sidecar metrics setting
- **THEN** it injects neither a metrics environment value nor a metrics container port

### Requirement: The agent SHALL admit workloads within aggregate limits

The agent SHALL verify the owner signature and decrypt every admission request addressed to its KEM
key, then reserve CPU, memory, and storage against its configured aggregate capacity. It SHALL
refuse admission beyond `max_workloads`, or beyond its CPU, memory, storage, reservation, payload
size, or replay limits.

#### Scenario: Admission beyond capacity is refused

- **WHEN** an admission request would exceed any configured aggregate limit
- **THEN** the agent refuses it and reserves nothing

#### Scenario: Replayed admission is refused

- **WHEN** an admission request reuses a nonce still retained in the agent's replay cache
- **THEN** the agent refuses it

Replay history is bounded and process-local. Eviction or restart can remove a nonce before the
request expires; duplicate refusal is then not guaranteed, although signature, target, freshness,
and operation-specific state checks remain mandatory.

#### Scenario: Request encrypted to another agent is unusable

- **WHEN** an agent receives a payload encrypted to a different agent's KEM key
- **THEN** decryption fails and the request is refused

#### Scenario: Request addressed to another agent is refused

- **WHEN** a request names a different agent as its target
- **THEN** the agent refuses it and reserves nothing

### Requirement: The agent SHALL refuse prohibited privilege and host-access manifest settings

The agent SHALL evaluate every manifest against a deny-by-default pod security policy before
deploying it, covering `initContainers` and `ephemeralContainers` as well as `containers`. The policy
SHALL refuse privileged containers, privilege escalation, added capabilities on any container other
than the injected sidecar, `runAsUser: 0`, `hostNetwork`, `hostPID`, `hostIPC`,
`shareProcessNamespace`, `hostUsers`, host ports, and **volumes of every kind**.

Volumes are refused wholesale for the first release. The agent drives a Podman socket that is
equivalent to host control, so a single `hostPath` entry is a complete escape, and admitting the safe
subset needs per-tenant naming rules that do not exist yet.

Only the container the agent injects, matched by its exact name, may hold `NET_ADMIN`. A tenant
container using a similar name SHALL NOT inherit that exemption.

These manifest checks SHALL NOT be described as proof of host confinement or tenant network
isolation. Direct application traffic on the shared workload network is not governed by proxy
credential checks, and the host and Podman runtime remain trusted execution boundaries.

#### Scenario: A host escape attempt is refused

- **WHEN** a manifest requests a host namespace, a host path, a host port, extra capabilities, or
  privileged execution
- **THEN** the agent refuses the deployment and names every violation

#### Scenario: A privileged init container is refused

- **WHEN** the privileged request is in `initContainers` rather than `containers`
- **THEN** it is refused just the same

#### Scenario: A tenant container cannot borrow the sidecar exemption

- **WHEN** a tenant names one of its own containers so as to resemble the injected sidecar
- **THEN** its added capabilities are still refused

### Requirement: Resource accounting SHALL cover every container that runs tenant code

Injected resource defaults and measured limits SHALL include `initContainers`, so a reservation
reflects everything the pod can consume.

#### Scenario: Init container resources are counted

- **WHEN** a manifest declares an init container without resource limits
- **THEN** defaults are injected for it and its limits are included in the measured total

### Requirement: Pending reservations SHALL NOT exhaust advertised capacity

An admission reserves resources before anything is deployed, and the requested
amounts are not checked against a real workload until the deployment grant
arrives. Reservations SHALL therefore be bounded twice: by how many one namespace
may hold at once, and by the share of capacity all pending reservations together
may hold.

Running workloads SHALL NOT be bounded this way. Consuming an agent by actually
deploying to it is what agents are for; holding it with admissions that never
deploy is not.

A workload larger than the pending share cannot be admitted on that agent, so the
share is configurable and an agent should be sized larger than the workloads it
is expected to host.

#### Scenario: An unfinished admission cannot zero out availability

- **WHEN** a namespace reserves as much as the pending share allows
- **THEN** the agent still advertises capacity to other clients

#### Scenario: One namespace cannot hold every reservation slot

- **WHEN** a namespace holds its maximum outstanding reservations
- **THEN** further reservations from that namespace are refused while other
  namespaces are still admitted

### Requirement: The workload network SHALL NOT be treated as a tenant boundary

All workload pods on an agent share one runtime network, so a pod can reach
another tenant's pod by address. Tenants are separated by owner identity at the
proxy — relay admission, route ownership, and egress are each gated on the owner
key — never by network topology.

Layer-3 isolation is outside this PoC's implementation scope, not inherently incompatible with
multi-host deployment. The absence of a network boundary SHALL be stated rather than implied away.

#### Scenario: The shared network is not relied on for isolation

- **WHEN** two tenants deploy to one agent
- **THEN** their pods share a network
- **AND** direct access to application ports is not isolated by Podmesh
- **AND** naming another owner does not authorize its mesh operations without its valid credentials

### Requirement: The agent SHALL report an owner's workloads on request

The agent SHALL answer an owner-signed list request with the workloads it holds
for that owner. A client keeps the only index of where it placed replicas, so
without this a lost or outdated index leaves workloads running that nothing can
address.

The request carries nothing secret and is not bound to one agent, so a scheduler
can broadcast it to agents the client cannot name — which is the case that
matters, since a client with no index does not know which agents to ask. Each
answer SHALL be sealed to the requesting owner, so relaying does not reveal the list contents.
The public request and transport metadata are not confidential.

An agent SHALL report only workloads belonging to the signing key.

#### Scenario: An owner finds workloads its local index lost

- **GIVEN** workloads running for an owner whose local index no longer lists them
- **WHEN** the owner asks the mesh
- **THEN** every such workload is reported

#### Scenario: Listing reveals nothing about other tenants

- **WHEN** an owner lists workloads on an agent hosting several tenants
- **THEN** only that owner's workloads are reported

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

### Requirement: The agent SHALL execute workloads through Podman with an injected sidecar

The agent SHALL decrypt the target-bound deployment grant, inject the configured sidecar image with
its metadata, and deploy the pod through Podman. Sidecar metadata SHALL be delivered to the sidecar
container alone, through the `PODMESH_SIDECAR_METADATA_B64` environment variable, so that no other
container in the pod receives the tenant material.

#### Scenario: Sidecar receives tenant material

- **WHEN** a pod is deployed
- **THEN** its sidecar metadata carries the tenant owner public key, the workload name, the derived
  routing key, the owner-signed workload credential, the proxy endpoint records, the tenant's
  workload relay token, and the relay CA certificates

#### Scenario: A workload whose credential does not verify is not started

- **WHEN** the execution specification carries a workload credential whose signature does not verify
  against the owner key, or which names another owner or routing key
- **THEN** the agent refuses the deployment rather than starting a pod whose sidecar the proxies
  would reject
- **AND** an expired credential is not a reason to refuse, because expiry is the proxy's decision
  and a pod may outlive the credential it was deployed with

#### Scenario: The routing key is derived, not chosen

- **WHEN** the agent builds sidecar metadata
- **THEN** the routing key is derived from the tenant owner key and workload name, so a proxy can
  later check that whoever registers it holds that owner key

### Requirement: The agent SHALL serve lifecycle commands only to the owner

Status, logs, and delete commands SHALL be accepted only when signed by the key that owns that
workload, addressed to this agent, and encrypted to it. All lifecycle traffic SHALL arrive over
Iroh; the agent's HTTP surface SHALL expose only `GET /health`.

A refusal SHALL NOT reveal whether the workload exists. "Not present" and "owned by somebody else"
SHALL be indistinguishable to the caller, on the lifecycle path and on the deployment path alike.

#### Scenario: Non-owner lifecycle command is refused

- **WHEN** a signed command from a key that does not own the workload arrives
- **THEN** the agent refuses it and does not disclose workload existence

#### Scenario: A failed deployment is not an existence oracle

- **WHEN** a deployment grant names a workload id belonging to another owner
- **THEN** the refusal is identical to one naming an id that does not exist at all

#### Scenario: HTTP surface is minimal

- **WHEN** any HTTP path other than `/health` is requested from an agent
- **THEN** the agent does not serve it

### Requirement: Persisted state SHALL be readable only by the agent

The encrypted workload store and its directory SHALL be created with owner-only permissions. The
records are ciphertext, but the file still discloses how many workloads an agent runs and how they
churn.

#### Scenario: The state file is not world readable

- **WHEN** the agent opens its store
- **THEN** the state file and its directory are owner-only

### Requirement: The agent SHALL bootstrap scheduler attachments over HTTP

The agent SHALL accept scheduler HTTP URLs, fetch and verify each scheduler's signed
`EndpointRecord`, and merge the results with any explicitly configured endpoints. The number of
bootstrap URLs SHALL be bounded, and each response SHALL be refused for size *while* it is read
rather than after it is buffered. A URL that does not answer SHALL be a startup error.

#### Scenario: Unreachable scheduler URL fails startup loudly

- **WHEN** a configured bootstrap URL cannot be resolved
- **THEN** the agent exits with an error rather than starting half-configured

#### Scenario: An oversized bootstrap response cannot exhaust memory

- **WHEN** a bootstrap URL streams a response beyond the size bound
- **THEN** the agent stops reading and fails that URL

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

### Requirement: Agent tests SHALL cover ownership and failure recovery

Agent changes SHALL be verified with focused ownership, accounting, lifecycle and restart tests
appropriate to the touched behavior. Real Podman execution SHALL be reported separately from mock
runtime tests or compilation; an unexecuted test SHALL NOT be reported as passing.

#### Scenario: An agent cannot be reached during reconciliation

- **WHEN** a named agent fails to answer a recovery test
- **THEN** the view remains explicitly incomplete
- **AND** restoration is verified before that scenario can pass

#### Scenario: Agent state is destroyed

- **WHEN** an agent and its durable keys are lost rather than temporarily unavailable
- **THEN** project documentation does not claim relocation, self-healing, or agent-loss recovery
