# Verification

## Environment

Linux aarch64, Podman 6.1.4 rootless/netavark. A temporary pod-sandbox attachment probe verified aliases and was removed. Started the previously inactive user Podman socket for stack tests and stopped its socket/service afterward. Final inspection found no running test containers or remaining managed/test-prefix networks.

Initial local stack execution used unoptimized native debug binaries, layered with their native loader/libraries onto existing local scratch images under `/tmp/podmesh-network-images`. An initial scratch-image attempt lacked the library search path; stopped it, removed its owned resources and corrected the temporary images before testing. The subsequent reset investigation rebuilt native runtimes with dev opt-level 3 (debug assertions retained), first as a pre-fix control, then with the two sidecar fixes. Final successful checks used these optimized runtime images, not the initial unoptimized ones. This is not verification of the official musl/release image-build pipeline; see investigation.md for the exact build command and comparison.

## Completed checks

- `cargo test --locked -p podmesh-agent runtime:: --lib`: 9 passed, including property tests and ownership/configuration rejection.
- Configuration test: default prefix, custom prefix, and refusal of the removed single-network CLI option passed.
- `cargo test --locked -p podmesh-integration-tests --features podman-tests --test tenant_networks`: passed twice; final version also verifies retention after the last workload is deleted. Exercises concurrent creators, distinct tenants, exclusive attachments, replacement/lifecycle and unmanaged-name refusal.
- Live Podman two-tier PostgreSQL test: passed, including workload DNS and a real query across distinct agent identities.
- `cargo test --locked -p podmesh-integration-tests --features podman-tests --test podman_transparent_egress_test`: passed before and after the proxy-restart/address-readiness fixture fix (107.5s and 73.22s).
- `cargo test --quiet --locked --workspace`: passed on the sequential rerun (exit 0), including the process-based stack and documentation tests.
- Workspace Clippy and Podman-feature Clippy with `-D warnings`: passed. Existing dependency future-incompatibility notice remains.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- Final focused documentation suite: 8 passed.
- `cargo check --locked -p podmesh-integration-tests --features kvm-tests --bin kvm-spike`: passed; this is compilation, not VM execution.
- `openspec validate add-per-tenant-workload-networks --strict --no-interactive`: passed.
- Repository-wide strict OpenSpec validation remains blocked by existing long-requirement warnings and an untouched missing scenario in workload-traffic-plane. Unrelated specifications were not rewritten.

## Follow-up execution

- An initial concurrent workspace/stack run collided on a host port. The separate workspace rerun passed; no assertion was weakened.
- First full-stack run passed initial ingress, tenant-sidecar metrics, lifecycle/discovery and three-replica deployment, but timed out waiting for sequential-update enrollment. Sidecar diagnostics also showed stale pre-attachment proxy addresses. Updated the test-only fixture to restart proxy sandboxes before grants and require signed records advertising the tenant-network address.
- Initial unoptimized full-stack rerun (`--test complete_rootless_stack`): 2 passed, 1 failed, exit 101 (402.85s). Initial ingress, tenant metrics, lifecycle/discovery, three-replica deployment and uninterrupted sequential updates passed, but `initial workload-local DNS/peer routing after update` failed with `Connection reset by peer`. Diagnostics included peer setup timeouts/closed connections, `replica catalog is not authorized locally`, and relay-2/3 TLS `BadSignature` warnings. Offline-proxy/expired-replica phases were not reached in this failed run. This is historical failed execution, superseded by the successful checks below.
- Rootful Podman and actual KVM mesh execution have not been performed. The KVM harness requires an x86_64 environment with its documented prerequisites.

## Reset investigation and final successful checks

The user requested investigation of the reset. Two focused regressions failed before fixes and pass afterward: an old sibling seed could replace the local execution catalog, and one failed sync could close a shared transport and reset an established service tunnel. Fixed both narrowly; preserved authentication, bounds, deadlines and existing integration assertions. Details and diagnostic limitations are in investigation.md.

- Optimized **pre-fix** full-stack control: passed in 311.86s, proving build sensitivity rather than an inherent tenant-network reachability failure.
- Fixed-source workspace suite: exit 0.
- Fixed optimized runtime images, real rootless Podman suite: **5 passed**, exit 0. `complete_rootless_stack`: 3 passed in 389.55s; transparent egress: 1 passed in 54.28s; tenant networks: 1 passed in 34.74s. All post-update DNS, proxy-offline and expired-replica phases completed.
- Final workspace and Podman-feature Clippy (`-D warnings`), formatting, `git diff --check` and owning-change strict OpenSpec validation: passed.
- An earlier fixed-image run was interrupted without a completion status during a host/user-runtime disruption; it is not counted as passing. Cleaned the run-owned stale infrastructure/network state and repeated the suite in foreground commands with explicit exit statuses after session recovery.
- Final cleanup found no test containers or managed tenant networks; stopped the test-started API socket/service. No source test assertion or runtime timeout was relaxed.
