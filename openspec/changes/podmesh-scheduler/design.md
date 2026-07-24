## Context

The scheduler previously combined placement, execution, durable records, key custody, and P2P
coordination. Running workloads must not depend on scheduler survival or expose workload data to it.

## Goals / Non-Goals

**Goals:** an interchangeable placement endpoint with no durable state and no workload information.

**Non-Goals:** admission, execution, workload CRUD, recovery, or Kubernetes API compatibility.

## Decisions

- Agents publish self-signed, expiring advertisements. This avoids a scheduler-owned node database.
- Selection uses coarse load then node ID. Determinism and simplicity are preferred over advanced
  optimization for the single-agent milestone.
- The registry is bounded and in memory. Scheduler restart intentionally discards it.
- Workload requirements are sent only in encrypted agent admission requests, not to the scheduler.

## Risks / Trade-offs

- [Registry empty after restart] -> agents republish every ten seconds.
- [Stale availability] -> selected agents perform authoritative admission and reject conflicts.
- [Any self-signed agent may advertise] -> namespace trust/attestation policy remains a client-side
  future extension; selected execution inherently grants that agent plaintext access.

## Migration Plan

Replace the old multi-role image and API entirely. No backward compatibility is maintained.