# Hardening Reassessment (2026-09-09)

## Implemented After Approval

The user subsequently requested "implement this", approving tasks 1.1/1.2 and the recommended
2.1/2.2 deferrals. The reassessment below is historical context, not the current implementation
status. No multi-host environment was provided.

Implemented durable planned-target create/cleanup state, charged accounting until confirmed
runtime and durable record removal, drop-safe per-workload mutation guards, cleanup retry/restart,
owner-visible pending state and old Active/Updating encoding compatibility. Uncertain remote
Podman completion is explicitly not inferred from a killed local command: its pending record and
capacity remain held for operator repair. No unsafe force-clear endpoint was introduced.

Implemented bounded optional HTTP proxy head parsing with httparse (16 KiB, 64 headers, five
seconds), semaphore-before-spawn (64), owned JoinSet handler shutdown, one-second queue/error-write
bounds and preserved buffered HTTP/CONNECT bytes. Removed raw request diagnostics. Existing
wire formats, owner/grant validation and raw-relay limits are unchanged.

Regression-first results: partial-create cleanup, invalid HTTP/CONNECT heads and interrupted-delete
retry tests failed before their fixes and passed afterward. The agent suite now includes 256
fixed-seed cleanup traces; HTTP tests include 256 fixed-seed fragmented-head/body cases, shrinking
enabled. Existing deployment/update and traffic integration tests remain green. Source specs and
PoC recovery documentation were synchronized after focused verification.

### Final Implementation Checks

- Agent library: 44 tests passed, including the 256-case cleanup trace property. Sidecar library:
  19 tests passed, including 12 HTTP admission tests and the 256-case fragmented-head property.
- `cargo test --quiet --locked --workspace --exclude podmesh-integration-tests`: passed across all
  runtime/shared crates and their tests. No failures or ignored tests in that run.
- `cargo test --quiet --locked -p podmesh-integration-tests --test podctl_deployment_flow --test podctl_update_flow --test sidecar_integration --test proxy_trust_flow --test workload_control_properties`:
  29 tests passed across the selected owner/traffic/security flows.
- `cargo clippy --quiet --locked --workspace --all-targets -- -D warnings`: passed.
- `cargo check --quiet --locked -p podmesh-integration-tests --features podman-tests --tests`: passed;
  this is compilation, not real Podman execution.
- Touched-file formatting and whitespace passed. Strict OpenSpec validation passed all 13 current
  items; all five proposed hardening requirement bodies match source specs after whitespace normalization.
- The required full `cargo test --quiet --locked --workspace` was attempted and stopped at
  `project_documentation::executable_sources_do_not_load_retired_workflow_records`: a duplicate
  `tests/tests/release_documentation.rs` is present and contains retired-path guard strings. It was
  not edited or deleted during hardening. Seven other project-documentation tests passed, including
  the updated recovery-scope assertions. That attempt was not green; the follow-up below resolves it.

The initial HTTP test compile needed error inspection without a Debug implementation for sensitive
request data; it passed after that test-only repair. Existing unrelated test/config changes were
preserved. No real Podman or multi-host execution was performed; no hosts/images/socket validation
is inferred from these mocks, TCP/Iroh tests or feature compilation.

### Documentation Blocker Resolved (2026-09-09)

The user explicitly requested removal of the duplicate legacy documentation test. Compared it with
`tests/tests/project_documentation.rs`: it had no unique current coverage, only stale helper and
pre-hardening scope assertions. Deleted `tests/tests/release_documentation.rs` and added its path
to the existing removed-surface guard without changing runtime code or weakening current tests.

- `cargo test --quiet --locked -p podmesh-integration-tests --test project_documentation`:
  all eight tests passed.
- `cargo test --quiet --locked --workspace`: the full default suite completed successfully, with
  zero failures or ignored tests. The earlier duplicate-test blocker is resolved.

These results do not claim real Podman or multi-host execution.

### Remaining Scope

Tasks 1.1/1.2 are implemented; the user-approved recommendations defer broad metrics errors and
connection-context extraction (2.1/2.2). Task 3.1 remains open pending actual hosts and operator
authorization. Remote Podman uncertainty deliberately retains charged state indefinitely until
operator repair; no safe automatic completion proof or force-clear command is claimed. Manual host
intervention must not discard sibling records. The documentation-test conflict is resolved by the
verified follow-up above. User acceptance of the delivered code is not inferred
from approval of its implementation plan.

## Historical Reassessment

The user requested checking the open tasks, verifying relevance and proposing implementation.
Only this change's proposal/design/tasks/spec deltas were edited. Runtime code and source specs
are unchanged, and no implementation or deferral approval is inferred.

## Current Findings

- Agent deploy failure still removes in-memory/durable records before confirming runtime cleanup.
  The final-save failure path also frees accounting before deletion succeeds. Current mock deploy
  failure happens before workload insertion, so it cannot prove the partial-create invariant.
- Agent control dispatch is cancellable; cleanup cannot rely on getting time after a runtime
  timeout. A starting row has no runtime ID, and delete can skip runtime deletion while create
  continues. The proposed fix includes precomputed target identity, pending phases and same-workload
  serialization. A remote timeout remains uncertain unless backend completion/removal is established.
- Restart already retains/quarantines problematic rows. The source spec's drop-and-release wording
  is stale and is corrected only in the proposal delta, pending implementation review.
- The explicit HTTP proxy is opt-in and normally bound to 127.0.0.1. It still lacks pending-task,
  request-head and queue-handoff deadlines/byte bounds; buffered-byte handoff and URL parsing fixes
  from the earlier refactor remain intact.
- Metrics classifiers still inspect display strings, but run after operation results; no decision
  to authorize a request is made there. The old proxy early-return classification bug is already
  fixed and its regression passes. A broad error-type migration is proposed for deferral.
- Sidecar connection arguments can be grouped using existing ProxySession, but no standalone
  correctness defect is demonstrated by those signatures. Context extraction is optional.
- Existing direct/relay tests run with local endpoints; the full-stack test uses a local Podman
  environment. These do not demonstrate independent hosts. No multi-host environment was supplied.

These are current-code observations, not newly reproduced partial-Podman failures or network
attacks. Proposed missing fault and bounds regressions must first fail against the old behavior.

## Executed Baseline

- `cargo test --quiet --locked -p podmesh-agent -p podmesh-sidecar --lib`:
  33 agent and 10 sidecar tests passed.
- `cargo test --quiet --locked -p podmesh-proxy --lib invalid_and_replayed_envelopes`:
  the real-Iroh metrics regression passed (26 unrelated unit tests filtered).
- `cargo test --quiet --locked -p podmesh-proxy --test workload_relay`:
  both direct/relay tests passed.
- `cargo test --quiet --locked -p podmesh-integration-tests --test podctl_deployment_flow --test podctl_update_flow`:
  nine deployment and one update test passed.

Total: 56 selected tests passed, zero failures/ignored. They verify the retained baseline, not the
proposed missing cases. No source-wide runtime rewrite, real Podman execution, remote-host test,
fuzz/soak or release pipeline was run. OpenSpec validation is performed on the proposed artifacts.

## Recommendation

Approve the implementation scope for original tasks 1.1 and 1.2 in design.md, in that order. Defer
2.1 and 2.2 unless the local fixes expose a concrete need; keep 3.1 open until a disposable topology
and operator authorization exist. All five original tasks remain unchecked pending implementation,
an accepted deferral or actual demonstration. This is a proposal, not a new release gate.