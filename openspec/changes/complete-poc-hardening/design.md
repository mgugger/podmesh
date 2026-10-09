## Context

Implementation approval: the user requested "implement this" on 2026-09-09 after the recommendation
to implement 1.1/1.2, defer 2.1/2.2, and keep 3.1 environment-dependent. The reassessment below is
the planning record; implementation and verification updates supersede its pending-approval wording.

The current [decision index](../../decisions.md) and source specs govern this work. The
[code inventory](../../code-inventory.md) records the earlier review and eight completed fixes.
The greenfield scope revision removed the former release-evidence obligations. This record now
owns only runtime hardening, interface follow-ups and the real multi-host PoC demonstration.

## Goals / Non-Goals

Restore dependable failure accounting and bounded HTTP admission within existing component
ownership, then exercise the PoC across hosts. Do not turn the PoC into a reconciled
cluster or expand accepted compatibility/availability guarantees.

## Decisions

- Treat failed-deploy cleanup as a defect, not an in-memory-state limitation. Start with the
  runtime identifier/failure contract and retain accounting until cleanup is confirmed. Design the
  local transaction against a failing-runtime regression before extending behavior.
- Bound HTTP proxy request bytes, whole-header time and admitted tasks using the existing listener
  and tunnel ownership. Preserve prefetched bytes and test slow/oversized clients independently.
- Do not change wire fields merely to remove string comparisons. Typed error migration and sidecar
  context cleanup require local review and explicit behavior/compatibility decisions.
- Run focused normal tests and keep commands/results with the relevant change. Container and
  multi-host runs require current images and the intended environment; compilation is not execution.
- Shared networking, plain ingress and volatile proxy grants remain accepted limits per D-11/12.
  Verification is proportional under D-22; do not restore removed assurance work.

## Risks / Trade-offs

- Partial Podman creation can outlive a failed command; a mock that fails before side effects is
  insufficient. Include partial creation, cleanup failure, persistence failure and restart cases.
- Earlier test successes predate later code edits. Preserve them below without reusing their counts
  as a current test result.
- Multi-host readiness is not proved by several containers on one host. Record physical topology
  explicitly when exercising direct and relay paths.

## Historical Test Results

- 2026-09-08: rootless ARM64 local Podman integration package reported 82 passed, zero failed/ignored,
  with rebuilt `flow-check-20260908` images. It preceded the later lean-interface refactor.
- 2026-09-08: post-refactor default workspace suite reported 533 passed, zero failed/ignored across
  71 targets; workspace all-target Clippy and focused formatting passed. No Podman rerun in that pass.
- Existing `proc-macro-error2` future-incompatibility notice remains separate from passing Clippy.

## Reassessment (2026-09-09)

Status: implementation proposal ready for review; runtime code is unchanged. The user requested
relevance verification and a proposed implementation, not implementation approval.

- Task 1.1 remains relevant: `AgentService::deploy_inner` removes active/store tracking on deploy
  failure without runtime cleanup, and on final persistence failure before cleanup is confirmed.
  `PodmanRuntime` computes the pod name before execution but only returns it on success; command
  timeout and outer dispatch cancellation can prevent that return. Existing restart failure
  handling already retains records/accounting and is a useful local pattern to preserve.
- Proposed invariant: once runtime creation may have started, capacity and an actionable record
  remain until the runtime is confirmed removed or deployment is committed. Failure cleanup must
  use an identity available before the operation, not depend on an error carrying one. A mock that
  creates state then fails, blocks or fails deletion should disconfirm an incomplete fix.
- Task 1.2 remains relevant: `HttpConnectProxy::run` spawns per accepted socket without a permit,
  `handle_connection` reads lines/headers without byte or whole-header time bounds, and the tunnel
  channel send can wait indefinitely. Retain the previously fixed prefetched-byte handoff while
  bounding pre-tunnel work. Test oversized and slow headers and a full tunnel queue locally.

### Disposition of the Five Original Tasks

| Original task | Current relevance | Proposed action |
|---|---|---|
| 1.1 Failed-deploy tracking | High-priority correctness/accounting defect. Success-only runtime ID, early row removal and starting-delete behavior are still present. Outer dispatch timeout can cancel cleanup. | Implement first, including same-workload mutation guards and cancellation tests. |
| 1.2 HTTP proxy admission | High-priority boundedness gap when explicit HTTP proxy mode is enabled. Normally loopback-bound and opt-in, so not an unconditional public listener exposure. Head reads, handler tasks and queue sends are unbounded. | Implement second; scope the bound to pre-tunnel work, not all application traffic. |
| 2.1 Typed metrics classification | Still string-based in agent/proxy/sidecar, but used after operation results for telemetry, not authorization. Proxy early-return finalization has already been fixed and its real-stream regression passes. | Defer the broad migration; use typed errors locally for new cleanup/parser outcomes. Do not repeat the completed finalization fix or change wire reasons. |
| 2.2 Sidecar connection context | Eleven serve_connection arguments and repeated clones remain. ProxySession already groups connection/identity/grant/replay data. No behavior failure is established by argument count alone. | Optional follow-up, not a blocker. If later justified, pass ProxySession plus a small private stream environment; do not introduce a cross-service context framework. |
| 3.1 Multi-host demonstration | Still relevant to the idea. Existing direct/relay tests use loopback, and full-stack tests launch multiple components on one local Podman host. | Keep separate after fixes, pending actual hosts/topology and permission to run there. No evidence bundle or benchmark harness. |

These are recommendations for this proposal, not user-accepted deferrals. Original task identifiers
remain in tasks.md; no runtime implementation or multi-host task is marked complete by this review.

## Proposed Implementation: Agent Failure Accounting

### Local interface and state

Add a pure runtime-owned target-resolution operation to `WorkloadRuntime`, taking the validated
`WorkloadDeployment` and returning the runtime ID before execution. Podman reuses `pod_name` on
the final agent-injected manifest; MockRuntime uses the workload ID. Do not derive a deletion target
from unsigned tenant metadata or an arbitrary error string. Keep deploy's existing returned ID and
check agreement before committing. Update actual trait implementers and their tests together.

Add explicit pending-create and cleanup-pending phases to the existing per-workload store. Persist
the planned target and charge resources before the first runtime await; commit Active only after
runtime success and the final durable save. Do not overload an Active row with an empty runtime ID.
Append phase variants without changing existing serialized Active/Updating discriminants or record
fields; test previously encoded records. Coordinated upgrade remains the policy: no new backward
decoder framework or destructive database reset. Existing ambiguous starting rows must be retained
and resolved conservatively using their signed execution material, never silently discarded.

Use checked phase transitions under the current state lock to prevent deploy/update/delete from
overlapping for one workload. Do not hold the global lock across Podman or serialize unrelated
workloads. A live create cannot be reported successfully deleted just because its runtime ID was
empty. In-flight ownership is bounded by existing operation/workload limits and released on drop.

### Completion, failure and cancellation

- If the first pending-state save fails, do not execute Podman or claim runtime side effects.
- On a runtime error or final commit failure, retain charged state, mark cleanup pending when
  possible, and make a bounded targeted deletion attempt. Remove the durable row before releasing
  its in-memory charge; a failed store removal must remain retryable even if runtime deletion succeeded.
- Preserve the original operation error. Cleanup failure is additional contextual diagnostics, not
  permission to remove tracking and not a fabricated successful owner receipt.
- Do not depend on async cleanup executing after a future is dropped. The prewritten pending state
  must survive runtime/dispatch cancellation; test cancellation at each await. Use the existing
  finite runtime/dispatch budgets, leaving a recoverable row when cleanup cannot finish within them.
- Pending-create/cleanup records are visible as non-active owner state. On startup or explicit owner
  delete, resolve cleanup rather than blindly replay an uncommitted create. Status/delete failures
  retain state and capacity; retry must be possible without a background controller.
- A killed local CLI does not prove a remote Podman request stopped. Timeout is an uncertain outcome,
  not proof of absence. If backend completion/removal cannot be established, keep accounting and
  report repair required; do not promise recovery from a late remote create based solely on a
  momentary not-found response. Validate the normal partial-failure path with real Podman separately.

The existing restart code already retains readable-but-unrestorable records and quarantines
unreadable ones. Preserve that behavior and update the contradictory source-spec scenario that
currently says to drop the record and free its resources. Unknown resource usage stays conservative;
existing healthy workloads remain available even when new admissions must stop.

### Focused tests

Extend the test-local FaultStore with remove failures and add a test runtime that can create then
fail, block after creation, refuse delete, and signal operation progress with Notify/oneshot barriers.
The current MockRuntime failure occurs before insertion, so it cannot prove partial-create cleanup.
Test initial-save failure, partial create, runtime timeout, outer cancellation, final-save failure,
cleanup failure, store-remove failure, retry, restart and sibling isolation. Exercise delete/update
while create is blocked and assert no premature release or success. Model the invariant with bounded
generated transition traces: uncertain runtime state implies retained record and charged capacity.

## Proposed Implementation: Local HTTP Proxy Admission

Use private named bounds, not new public configuration knobs for this PoC:

| Bound | Proposed value |
|---|---|
| Combined request line and header bytes | 16 KiB |
| Parsed headers | 64 |
| Whole-head deadline from socket acceptance | 5 seconds, not reset per line |
| Concurrent pre-tunnel handlers | 64 |
| Tunnel-queue handoff wait | 1 second |
| Best-effort error-response write | 1 second |

Use the transparent egress listener's existing semaphore-before-spawn pattern locally. Reject an
excess socket immediately; track admitted handlers in a JoinSet and reap them so completed task
handles do not accumulate. Listener abort/shutdown must drop or drain its children. A handler owns
its permit through parsing and bounded queue handoff, then releases it. The existing bounded tunnel
queue, per-proxy stream permits and raw-relay limits keep their own distinct responsibilities.

Read into a byte-limited head buffer and parse with an established HTTP/1 head parser (`httparse`
as a small direct dependency), rather than unlimited `read_line` or a new HTTP framework. Reject
truncated/invalid heads and excessive headers before enqueueing. Preserve unread/prefetched bytes
when rebuilding the origin-form request or handing off CONNECT, remaining below the existing
64 KiB egress initial-data bound. Keep the existing URL parser and credential checks.

Use bounded best-effort 400 for malformed/truncated input, 431 for excessive heads and 503 when
queue handoff saturates; a deadline may simply close the socket, and overload before parsing closes
immediately. Never emit CONNECT 200 before the existing authorized remote setup succeeds. Stop
logging raw request lines/headers: rejected URL userinfo or sensitive query values must not enter
diagnostics. Preserve method/target handling within the existing supported proxy modes.

Tests use loopback sockets, tiny injected private limits and deterministic clocks where possible:
exact-limit/limit+1 heads, many short headers, byte-at-a-time slow headers, EOF before terminator,
full/closed tunnel queue, handler saturation, shutdown while reading/sending and permit recovery.
Retain both buffered HTTP-body and early CONNECT regressions; add fragmented-head property cases
under bounded generators and a reproducible seed. These are normal tests, not fuzz or soak jobs.

## Follow-up Options Outside the Recommended Batch

Typed metrics errors: if later selected, use owner-local typed categories recoverable through an
error source chain for deadline, saturation, replay and authorization; keep display wording and
wire fields stable. Add category tests through wrapping Context, and preserve one metric completion
per operation. Avoid deriving categories from rewritten strings or centralizing every error in a
new framework. Current classifiers are a maintenance weakness, not a demonstrated security bypass.

Sidecar context: reuse ProxySession for per-connection authority and a private environment holding
config/client/shared slots/metrics/cancellation. Keep the authenticated endpoint and stream handles
explicit. Do not merge process-wide and per-connection authority or expand public APIs for aesthetics.
Exercise registration, reconnect, unauthorized peers, ingress and egress if this refactor is taken on.

Multi-host demonstration: use three independent agent hosts for three replicas, two scheduler
entry points and reachable proxy/relay endpoints; some roles may share machines as documented.
Use actual advertised addresses, not local hostnames or shared state volumes. Record approved keys,
topology (not secrets), image versions and commands in a short change note. Verify apply/status/logs,
ingress/egress, scheduler restart and delete; exercise direct and relay-only paths in a disposable
environment with scoped, operator-approved network changes. Negative identity/tamper tests remain
local normal tests and should also be demonstrated over the selected cross-host control path.
No hosts, credentials or permission to disrupt network routes have been supplied in this review.

## Recommended Approval Scope and Order

Approve original tasks 1.1 and 1.2, including their local interfaces, rejection semantics and
regressions above. Implement agent failure accounting first, HTTP pre-tunnel admission second,
with each small failing test run immediately before/after its fix. Tasks 2.1/2.2 are proposed
deferrals; task 3.1 waits for a concrete test topology. No source spec or runtime code has been
changed to pretend the proposal is already implemented.

## Implementation Notes

- Runtime-owned `target_id` resolves the final injected Podman name before creating a durable
  Creating row. Appended Creating/CleanupPending variants preserve prior Active/Updating encodings.
- A bounded per-workload mutation guard is released on drop; the global state lock is not held
  during runtime awaits. Owner deletion retries retained cleanup and interrupted deletion markers.
- Runtime timeout/I/O completion uncertainty is typed. `cleanup_failed_deploy` refuses uncertain
  outcomes by default, including Podman, rather than interpreting an absent pod as safe removal.
  The in-process mock can establish completion and implements confirmed cleanup. Podman uncertainty
  intentionally requires operator repair; no unsafe automatic state-clear endpoint was added.
- HTTP admission uses httparse, byte-limited buffered reads, semaphore-before-spawn and JoinSet
  ownership. A single head deadline is retained across partial reads. Queue reservation precedes
  socket transfer and error writes have their own bound. Sensitive request text is not logged.
- New deterministic/property tests cover the approved failure paths. Real Podman and multi-host
  execution are distinct from mocks, local TCP/Iroh and feature compilation.