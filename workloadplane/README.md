# Beemesh Workload Plane Crates

The workload plane now ships as two cooperative binaries:

- `workloadplane/workload` (`workplane`) - the primary in-pod agent that maintains the workload DHT, self-heals replicas, surfaces REST health, and manages secure libp2p participation for the workload fabric.
- `workloadplane/gateway` (`workplane-gateway`) - a minimal pause-container sidecar that bootstraps into the workload DHT in **client mode**. It only performs Kademlia provider lookups for the current workload identity, never advertises or stores routing tables, and keeps the surface area intentionally tiny.

Each crate has its own `Cargo.toml` and can be built or tested independently via the workspace root:

```bash
cd /root/git/beemesh
cargo check -p workplane
cargo check -p workplane-gateway
```

Use the `workplane` binary when you need the full agent (self-heal, REST, raft hooks). Deploy `workplane-gateway` as the ultra-lightweight pause container when a pod needs DHT connectivity without the full controller.
