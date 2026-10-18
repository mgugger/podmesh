# Post-update peer reset investigation

User requested investigation of the remaining connection reset after tenant-network implementation.

## Observed failure

The namespace DNS query succeeds and yields a synthetic VIP. The subsequent HTTP read resets. Thus the displayed TCP reset is downstream of internal peer setup/forwarding, not evidence that DNS lookup or tenant bridge creation failed. The previous run logged `workload frame read timed out`, closed peer connections, and transient `replica catalog is not authorized locally` during rollout.

## Leads under test

1. The previous stack images contain unoptimized native debug binaries. Peer setup has a three-second deadline including signed-envelope/record validation and cache admission. Existing DNS-change verification records build-sensitive enrollment/discovery deadline failures resolved by optimized guest builds without increasing timeouts. Rebuild the same source with dev opt-level 3/debug assertions retained, no debuginfo/incremental, into `/tmp/podmesh-peer-opt-target`, then repeat the unchanged full-stack test.
2. During sequential update, podctl intentionally seeds updated replicas with old siblings. `ServiceCache::seed` unconditionally replaces the catalog; this can replace the new sidecar's own catalog with the old one. A later lease refresh then rejects the new own record. Establish this with a focused regression before changing semantics.
3. `PeerConnector::exchange` closes its cached connection on any setup failure, while the connection can carry concurrent sync and service streams. This can amplify one setup failure into active-stream resets. Do not change transport lifetime until a focused reproduction establishes the behavior.

## Confirmed defects and focused checks

- Extended the production sidecar/control/DNS test to submit an older, owner-signed sibling catalog for the sidecar's own manifest. Before the fix it failed in 3.26s: `an old sibling seed replaced this workload's own execution catalog`. `WorkloadNetwork::seed` now rejects such conflicts before changing cache or hint state; deployment metadata remains authoritative for the local catalog. Remote-catalog seeding remains supported.
- Added `peer_stream_isolation::failed_sync_does_not_reset_an_established_service_tunnel`, using two real Iroh endpoints and an echo application. First establish a valid service stream, induce a deterministic sync catalog conflict on its cached connection, restore control state, then exchange more bytes on the original tunnel. Before the fix it failed in 0.37s with `active tunnel reset by failed sync: connection lost: closed`.
- `PeerConnector::exchange` no longer closes the shared connection on per-operation setup failure. The failed operation's streams are dropped; normal transport closure detection, stream checks and timeouts remain in place. The same regression passes after the fix.
- Both focused regressions passed after the fixes, and `cargo test --locked -p podmesh-sidecar` passed all sidecar tests (including foreign-owner, replay, record/cache validation and proxy-free peer routing). Scoped Clippy passed with `-D warnings`.

## Original failure path and optimized control

The transparent egress handler owns the accepted local TCP socket but does not consume application bytes until peer setup succeeds. `WorkloadNetwork::forward` returns an error if no authorized remote replica can be opened and this is not its own service. Dropping that socket with the HTTP request still unread can surface as a TCP reset to the namespace probe. This explains why a successful VIP DNS answer can be followed by `Connection reset by peer`; it does not by itself prove a remote application reset. Separately, the stream-isolation regression proves collateral resets of already-established tunnels from connection-wide setup cleanup.

Built the pre-fix runtime source using `CARGO_PROFILE_DEV_OPT_LEVEL=3 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_INCREMENTAL=false cargo build --locked --target-dir /tmp/podmesh-peer-opt-target -j 4 -p podmesh-agent -p podmesh-scheduler -p podmesh-proxy -p podmesh-sidecar`, and layered its stripped native binaries onto the same local test images. No fixture topology, runtime deadlines or full-stack assertions changed for this control. The unchanged `complete_rootless_stack_serves_ingress` passed in **311.86s**, including post-update peer DNS/TCP, all-proxy outage, stopped-replica expiry and recovery/cleanup.

This establishes build-sensitive failure: the earlier unoptimized native test images failed while optimized pre-fix images passed. It is not a measurement proving a particular CPU-quota or mutex cause. The exact original reset is not attributed solely to the two independently reproduced defects. Official container builds already use optimized release binaries; the prior manual debug-image setup was not equivalent to those builds.

## Final verification

- `cargo test --quiet --locked --workspace`: passed (exit 0) after the fixes, including the strengthened stream-isolation regression and production DNS stale-seed refusal.
- `RUST_LOG=info,iroh=warn cargo test --locked -p podmesh-integration-tests --features podman-tests --test complete_rootless_stack --test podman_transparent_egress_test --test tenant_networks`: **passed**, exit 0. Complete stack: 3 passed in 389.55s (including live PostgreSQL, uninterrupted updates, peer DNS/TCP, all-proxy outage, replica expiry and restored ingress); transparent egress: 1 passed in 54.28s; tenant networks: 1 passed in 34.74s.
- Workspace and Podman-feature Clippy with `-D warnings`: passed. Formatting, diff whitespace and owning-change strict OpenSpec validation passed.
- One earlier fixed-image attempt was interrupted with no exit status during a host/user-runtime disruption; it is not counted as passing. After session recovery, inspected stale state, removed only the run's three infrastructure pods and unused tenant network, restarted the API socket and reran foreground commands with explicit exit-status files.
- Final inspection: no running/stopped test containers, no managed tenant networks left over. Stopped the test-started user Podman socket/service. Rootful and actual KVM execution were not performed.

No production deadlines, authentication checks, or existing integration assertions were weakened. Native optimized test images are not a verification of the official musl image-build pipeline. The optimized pre-fix control demonstrates that the original reset was build-sensitive; the two code fixes address independently confirmed failure mechanisms rather than claiming to prove a unique cause for every earlier reset.



