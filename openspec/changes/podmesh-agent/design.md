## Context

Execution, persistence, key release, and scheduling previously shared one binary. The new agent is
the only host component allowed to decrypt a selected workload. Every other node is treated as
untrusted, and loss of the last agent is an accepted terminal failure for the initial release.

## Goals / Non-Goals

**Goals:** isolate Podman and sidecar injection, encrypt local state, enforce owner-signed lifecycle
commands, and leave a narrow future replica-grant boundary.

**Non-Goals:** remote recovery, replica consensus, or a global desired-state store.

## Decisions

- Use direct HTTP endpoints with transport-neutral postcard records. This is simpler now and keeps a
  later Iroh migration within endpoint/network code.
- Persist the encrypted grant before starting Podman. Restart can safely retry an incomplete deploy.
- Keep active workloads and reservations in one atomically locked map. Signed reservations carry
  their resource footprint, allowing admission to enforce aggregate CPU, memory, storage, and
  workload-count limits without exposing requirements to the scheduler.
- Store each workload under its full workload ID and reconcile every encrypted record on restart.
- Recompute post-sidecar, policy-mutated container limits before deploy and restart. Reject a
  workload if actual CPU, memory, or ephemeral storage exceeds its signed reservation.
- Inject sidecars after decrypting at the agent, so the scheduler never receives manifest-derived
  metadata.

## Risks / Trade-offs

- [Agent host sees plaintext] -> unavoidable for non-confidential-computing execution; only the
  selected agent receives the DEK.
- [Last agent lost] -> owner must deploy again; future workloads can require replicas.
- [HTTP locator can become stale] -> advertisements expire and agents republish; future Iroh tickets
  replace only the locator mechanism.

## Migration Plan

Deploy the stateless scheduler, then agents with persistent key/state volumes, then switch `podctl`.
Old scheduler nodes cannot serve the new protocol and are removed rather than supported in parallel.