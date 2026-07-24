## Context

The previous execution design coupled scheduling, runtime ownership, and key custody. The current
system treats the scheduler as an untrusted stateless selector and allows each selected agent to host
many independently encrypted workloads.

## Goals / Non-Goals

**Goals:** keep workload plaintext and DEKs confined to the selected agent, enforce owner-bound
admission and lifecycle commands, account for aggregate resources, and recover all local workloads
after agent-process restart.

**Non-Goals:** remote recovery after durable agent loss, replica consensus, confidential execution
from the selected host, or a global desired-state API.

## Decisions

- `podctl` and the agent share one strict manifest policy and Kubernetes quantity parser. The client
  reserves measured application limits plus deterministic sidecar overhead; the agent remeasures the
  post-sidecar manifest before execution.
- Signed reservations include CPU, memory, and ephemeral storage amounts. Reservation-to-deployment
  transition occurs under the same state lock used for active workload accounting.
- Each workload is keyed by its full namespace-scoped workload ID in memory, encrypted redb storage,
  and Podman resource names. This prevents collisions between tenants using the same public name.
- Runtime deployment intent is persisted before Podman creation. Final runtime identity is persisted
  afterward; restart retries incomplete records and reconciles each workload independently.
- Public advertisements expose only coarse maximum utilization and availability. Workload-specific
  resource requirements remain encrypted to the selected agent.

## Risks / Trade-offs

- [Selected host sees plaintext] -> unavoidable without confidential computing; only that agent gets
  the wrapped DEK.
- [One corrupt local record can block startup] -> fail closed rather than silently omit a workload;
  future tooling can provide owner-authorized quarantine and repair.
- [No remote recovery] -> loss of the sole replica and agent keys requires owner redeployment;
  future replica handoff extends grants without adding scheduler state.
- [One pod-bearing document per workload] -> keeps one runtime ID per lifecycle record; future
  compound workloads require an explicit multi-runtime record rather than implicit partial cleanup.

## Migration Plan

Existing single-row agent state is not migrated because backward compatibility is not required.
Redeploy workloads through `podctl`; new records use the per-workload table and full workload IDs.