## 1. Protocol And Data Model

- [x] 1.1 Add bounded owner-signed update request and agent-signed update response records with
  target-agent, expected-revision, requested-revision, freshness, nonce, and response-key bindings
- [x] 1.2 Add protocol tests for cross-operation signatures, stale expected revisions, wrong targets,
  response substitution, size bounds, expiry, and replay
- [x] 1.3 Version the podctl deployment catalog so each replica records its last confirmed revision,
  receipt, agent endpoint record, agent KEM key, and update state
- [x] 1.4 Version the encrypted agent workload row with active and updating states that retain enough
  signed material to reconcile an interrupted replacement

## 2. Agent Enforcement And Persistence Hardening

- [x] 2.1 Add an explicit MVP manifest-kind allowlist for one Pod or Deployment plus supported Service
  and Ingress documents, rejecting additional pod-bearing, storage, Secret, controller, and unknown
  documents
- [x] 2.2 Enforce exactly one runtime pod per replica at the agent and reject non-normalized replica
  counts before reservation consumption and Podman execution
- [x] 2.3 Reject ephemeral containers for the MVP or include them fully in resource defaulting and
  measurement, with policy tests covering the chosen behavior
- [x] 2.4 Rework agent-store iteration to return per-key load results and isolate corrupt,
  undecryptable, malformed, or key-mismatched records during restart
- [x] 2.5 Create the agent state directory and database file with owner-only permissions at creation
  time and add Unix permission regression tests
- [x] 2.6 Add a named runtime-operation semaphore and complete-operation timeouts around deploy,
  update, status, logs, delete, reconciliation, and control responses
- [x] 2.7 Make deploy and update cleanup preserve the original error while always repairing in-memory
  state and reporting cleanup failures

## 3. Resource-Aware Placement

- [x] 3.1 Extend the scheduler selection HTTP query with bounded CPU, memory, storage, and capability
  criteria and reject invalid values before gossip
- [x] 3.2 Copy validated HTTP criteria unchanged into signed capacity queries and retain exact offer
  validation against them
- [x] 3.3 Make podctl send resources measured after canonical manifest validation and sidecar resource
  inclusion instead of placeholder probes
- [x] 3.4 Add bounded podctl retry for admission capacity races, excluding refused agents without
  retrying trust, signature, malformed-message, or policy failures
- [x] 3.5 Prevent equivalent concurrent selection callers from being forced onto one cached winning
  offer when multiple eligible offers are available
- [x] 3.6 Add scheduler and podctl integration tests proving a nearly full agent is skipped for a
  fitting agent and excess retries terminate with a precise error

## 4. Agent Update Transaction

- [x] 4.1 Add update admission accounting that subtracts the current workload reservation before
  testing whether the requested replacement fits aggregate capacity
- [x] 4.2 Validate update ownership, workload identity, target agent, expected revision, requested
  revision, reservation binding, execution identity, resource limits, and service-mesh material
- [x] 4.3 Return idempotent success when the requested revision is already active and a conflict when
  the stored revision matches neither expected nor requested revision
- [x] 4.4 Persist an updating transition before Podman replacement and commit the new active row only
  after runtime success
- [x] 4.5 Reconcile interrupted update transitions after agent restart without affecting sibling
  workloads
- [x] 4.6 Add mock and Podman-backed tests for successful replacement, unchanged revision, stale
  update, runtime failure, persistence failure, and restart during each transition

## 5. podctl Apply And Partial Update Recovery

- [x] 5.1 Make apply load or reconcile the stable deployment before deciding between create,
  idempotent success, and update
- [x] 5.2 Reject replica-count changes before granting proxies, selecting agents, admitting
  workloads, or changing catalog state
- [x] 5.3 Update replicas sequentially on their recorded agents and save each verified revision and
  receipt atomically
- [x] 5.4 Preserve failed replicas at their last confirmed revisions, report every per-replica
  outcome, and make retry skip replicas already at the requested revision
- [x] 5.5 Refresh proxy records, relay credentials, workload credentials, and sidecar metadata during
  manifest and credential-only updates
- [x] 5.6 Add end-to-end tests for unchanged re-apply, changed revision, fixed placement, partial
  update, retry, stale catalog revision, and refused scaling

## 6. Mesh-Wide Agent-Backed Reconciliation

- [x] 6.1 Define bounded signed scheduler reconciliation query and response coordination records with
  signer-bound reply endpoints, expiry, and deduplication keys
- [x] 6.2 Gossip reconciliation queries to admitted schedulers and have each scheduler fan out only to
  its local agent attachments with bounded concurrency
- [x] 6.3 Extend owner-sealed agent list responses with current signed receipts, agent endpoint and
  KEM material, revision state, and the data required to reconstruct lifecycle placements
- [x] 6.4 Aggregate sealed answers ephemerally, deduplicate agents reachable through multiple paths,
  and report unreachable schedulers and agents without persisting workload state
- [x] 6.5 Make podctl atomically rebuild or repair catalogs from verified reconciliation responses
  without deleting local entries from a partial view
- [x] 6.6 Add multi-scheduler tests where reconciliation through a scheduler with no attached agents
  rebuilds a deleted catalog and enables status, logs, update, and delete
- [x] 6.7 Add a scheduler-restart test proving workloads remain on agents and fresh reconciliation
  succeeds after reattachment without scheduler-held workload state

## 7. Mandatory Service-Mesh Continuity

- [x] 7.1 Keep production apply and update fail-closed when proxy records, relay credentials, proxy
  grants, workload credentials, or injectable sidecar metadata are missing or invalid
- [x] 7.2 Add bounded renewal-window logic for proxy grants and workload credentials before expiry
- [x] 7.3 Implement credential-only replica updates that preserve workload identity, placement,
  routing key, and application manifest revision
- [x] 7.4 Report per-proxy and per-replica renewal failures while retaining existing unexpired
  authority
- [x] 7.5 Add an explicit test-harness-only service-mesh bypass that cannot be selected by tenant
  manifest data or production defaults
- [x] 7.6 Add workload-plane tests covering continued registration and reconnect after credential
  renewal, partial renewal failure, and refusal after final expiry

## 8. MVP End-To-End Validation

- [x] 8.1 Extend the in-process mesh suite to cover resource-aware placement, update, partial failure,
  reconciliation, corrupt-record isolation, and runtime concurrency saturation
- [x] 8.2 Extend the Podman suite to deploy and update a single replica through a scheduler that holds
  no agent attachment, then verify status, logs, ingress, egress, reconciliation, and deletion
- [x] 8.3 Add a three-replica Podman scenario proving distinct placement, sequential update, shared
  routing, traffic continuity through available replicas, and complete deletion
- [x] 8.4 Add adversarial tests for replica-count bypass, unsupported manifest documents, stale
  updates, wrong-owner updates, tampered ciphertext, replay, oversized reconciliation, and bounded
  fanout
- [x] 8.5 Run formatting, Clippy with warnings denied, workspace tests, and the Podman feature suite

## 9. Documentation And Release Contract

- [x] 9.1 Add a glossary defining owner, namespace, tenant, deployment, workload, revision, replica,
  scheduler attachment, and reconciliation
- [x] 9.2 Document supported MVP manifest kinds, mandatory service-mesh inputs, fixed-count update
  semantics, partial update recovery, and credential renewal
- [x] 9.3 Replace the broken minimal quick start with tested commands and remove or mark stale
  manual-sidecar sample manifests
- [x] 9.4 Document that schedulers retain no workload state, reconciliation asks agents, updates do
  not move replicas, and destroyed agent state is unrecoverable
- [x] 9.5 Verify every README workflow against the final CLI and end-to-end tests
