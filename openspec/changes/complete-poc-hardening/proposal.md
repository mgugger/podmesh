## Why

Reassessment on 2026-09-09 confirms two remaining PoC defects: failed deployment can lose
accounting/tracking before cleanup, and the optional HTTP proxy admits unbounded pre-tunnel work.
Both undermine a useful multi-workload demonstration even without production guarantees.

Status: implementation authorized on 2026-09-09 by "implement this" after the presented plan.
Tasks 1.1 and 1.2 are the implementation scope; broader refactors are deferred and multi-host
execution awaits a supplied environment. See verification.md for checks actually performed.

## What Changes

- Retain an actionable runtime identity and charged pending state before deployment can have side
	effects; serialize same-workload mutations and release accounting only after confirmed cleanup.
- Make failure, timeout, cancellation and restart paths honor that invariant, without a controller
	or relocation. Correct the source spec's stale instruction to drop unreadable records on restart.
- Bound optional local HTTP proxy admission, head parsing, queue handoff and handler shutdown while
	preserving prefetched bytes, endpoint credentials and existing raw-relay limits.
- Defer broad typed-metrics errors and optional sidecar context extraction for this proposed batch.
	Keep the actual multi-host demonstration as a separate environment-dependent task.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `agent`: cancellation-safe deployment accounting, targeted cleanup and conservative restart state.
- `workload-traffic-plane`: bounded local HTTP proxy pre-tunnel work and preserved byte handoff.

## Impact

Agent service/runtime/store/control tests; sidecar HTTP proxy/listener integration and parser tests.
The runtime trait may gain a target-resolution method and local persisted phases will be extended;
owner-agent wire records, CLI commands and credential/grant semantics remain unchanged. Existing
active/update store records must remain readable without deleting user state or a migration framework.

## Non-Goals

No automatic relocation, desired-state controller, scaling, persistent proxy grants, consensus,
global denial-of-service protection, speculative compatibility framework or dedicated assurance
process. No wholesale error-system rewrite or context object merely to reduce argument counts.