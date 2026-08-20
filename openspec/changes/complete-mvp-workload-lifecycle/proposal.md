## Why

Podmesh has the core zero-trust deployment path, but its current contracts do not yet provide a
safe, repeatable MVP lifecycle: placement ignores measured resources, agent-side validation can be
bypassed by a non-standard client, duplicate apply can orphan workloads, and reconciliation is not
mesh-wide. Completing these guarantees now establishes a coherent workload platform in which
stateless schedulers remain blind while owners can deploy, update, rediscover, and operate replicas
through the mandatory service mesh.

## What Changes

- Make capacity selection use the workload's measured CPU, memory, storage, and required
  capabilities, with bounded retry when admission loses a capacity race.
- Enforce exactly one runtime pod per replica at the agent, restrict accepted manifest kinds, cover
  every executable container in resource accounting, and bound concurrent Podman operations.
- Make duplicate `podctl apply` an idempotent update: unchanged revisions succeed without work,
  changed revisions replace existing replicas on their current agents, and replica-count changes
  are rejected for the MVP.
- Add revision compare-and-swap binding so stale or concurrent updates cannot overwrite a newer
  workload revision.
- Persist partial update progress per replica so retries continue only the replicas that have not
  reached the requested revision.
- Make owner workload discovery mesh-wide without giving schedulers durable workload state, and let
  `podctl` rebuild or repair its local catalog from verified agent responses.
- Keep sidecar injection and proxy participation mandatory for production workloads, while allowing
  an explicitly test-only service-mesh bypass for isolated scheduler and agent tests.
- Define renewal of proxy grants and workload credentials so long-running workloads do not silently
  lose traffic-plane authorization.
- Harden encrypted agent persistence so one corrupt record cannot prevent other workloads from
  being restored and state files are owner-only from creation.
- Clarify that scheduler state is ephemeral, agent loss is not recovered remotely, and local agent
  reconciliation does not move workloads between agents.

## Capabilities

### New Capabilities

- `workload-update`: Idempotent apply, revision-bound in-place replica updates, partial-progress
  recovery, and fixed replica count for the MVP.

### Modified Capabilities

- `podctl-cli`: Send measured placement requirements, update existing deployments, reconcile the
  local catalog from agents, and keep production service-mesh configuration mandatory.
- `scheduler`: Relay resource-aware placement and mesh-wide owner discovery without retaining
  workload state.
- `agent`: Enforce single-pod execution, strict manifest scope, complete resource accounting,
  bounded runtime concurrency, safe updates, and per-record persistence recovery.
- `message-security`: Add target-bound, owner-signed update messages with revision compare-and-swap
  semantics.
- `workload-traffic-plane`: Keep the service mesh mandatory and renew the expiring grants and
  credentials used by long-running workloads.

## Impact

- `podctl`: apply/update decision logic, replica catalog schema, placement request parameters,
  reconciliation, credential renewal, and CLI error reporting.
- `podmesh-scheduler`: selection API criteria, candidate/retry behavior, and bounded mesh-wide list
  coordination.
- `podmesh-agent`: admission/update protocol handling, manifest validation, Podman replacement and
  concurrency control, encrypted store loading, and reconciliation semantics.
- `podmesh-proxy` and `podmesh-sidecar`: authorization renewal and test-only bypass wiring.
- `shared/protocol` and `shared/crypto`: update records, revision binding, bounds, and validation.
- Integration coverage and operator documentation for deploy, update, partial failure,
  reconciliation, scheduler restart, service-mesh traffic, and unrecoverable agent loss.
