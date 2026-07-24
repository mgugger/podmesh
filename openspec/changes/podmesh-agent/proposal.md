# Podmesh Execution Agent

## Purpose

Move all host-local workload logic out of the scheduler. The agent owns encrypted admission,
Podman execution, sidecar injection, local encrypted state, status, logs, and deletion.

## Scope

- Many active workloads per agent, bounded by `max_workloads` and aggregate CPU, memory, and storage
	capacity.
- Direct encrypted communication with `podctl` over HTTP.
- Persistent Ed25519/X25519 node keys and one encrypted redb record per workload.
- Local restart reconciliation only; no remote recovery or replica handoff.

## Future Boundary

Replica handoff can add a replica-signed deployment grant without adding workload state to the
scheduler. Transport code is isolated so HTTP can later be replaced by Iroh endpoints.