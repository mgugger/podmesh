## 1. Known Runtime Defects

Approved on 2026-09-09 by "implement this" for the presented two fixes and recommended deferrals.
The implementation details are in design.md and checks/limitations are in verification.md.

- [x] 1.1 Implement agent failure accounting: planned runtime targets, durable charged pending phases, mutation guards and retry/restart cleanup. Partial-create, cancellation, persistence, delete-retry, old record encoding, uncertain backend, visibility and sibling tests pass, including generated cleanup traces. Owner deploy/update flow tests pass and the source delta is published. Uncertain remote Podman records deliberately remain charged for operator repair.
- [x] 1.2 Implement bounded HTTP admission: 16 KiB/64 headers/five-second head limit, 64 pending handlers, one-second queue/error writes and owned handler shutdown. Exact/excessive/fragmented/slow/truncated head, queue/saturation/shutdown/permit and buffered-byte tests pass; nine traffic integration tests pass and the source delta is published.

## 2. Interface Follow-ups

Both assessments are recorded in design.md. Approval of the recommended batch accepts these
deferrals. The assessment tasks are complete, not the optional implementations.

- [x] 2.1 Defer cross-component typed metrics classification as recommended and approved; only local runtime-uncertainty and HTTP-parser errors were added for these fixes. No wire reasons or broad metrics taxonomy changes.
- [x] 2.2 Defer optional sidecar context extraction as recommended and approved; no generic context/service abstraction introduced.

## 3. PoC Demonstration

- [ ] 3.1 Once actual hosts, addresses, trusted identities and operator-approved network test scope are available, exercise three fixed replicas on three agent hosts through two scheduler entry points, direct/relay connectivity, scheduler restart, ingress/egress, owner lifecycle and negative identity/tamper checks. Record topology and commands in a short change note; loopback and one-host Podman tests are not completion of this task.

## Verification Sequence

1. For 1.1, add discriminating fault/cancellation regressions before the fix, then run
	`cargo test --locked -p podmesh-agent --lib` and
	`cargo test --locked -p podmesh-integration-tests --test podctl_deployment_flow --test podctl_update_flow`.
2. For 1.2, add boundary/slow/queue tests before the fix, then run
	`cargo test --locked -p podmesh-sidecar --lib http_connect_proxy` and
	`cargo test --locked -p podmesh-integration-tests --test sidecar_integration`.
3. After both approved fixes, run touched-file formatting, workspace warning-denied Clippy, the
	locked default workspace suite and strict OpenSpec validation. Compile Podman-enabled tests;
	run actual Podman cases only with the required images/socket, reporting execution separately.
4. Keep results/failures and remaining limitations in verification.md. No fuzz, soak, release
	providers, benchmark reports or certification tasks are reintroduced.

The former six release-evidence tasks were removed by the user-approved greenfield scope change
on 2026-09-09, not completed. Verification of each remaining fix uses normal tests with the change.