# Podman Deployment

Create the workload network and make the Podman socket available:

```bash
systemctl --user start podman.socket
podman network create podmesh
./deploy/build_containers.sh
```

The build creates scratch-based images only for the native host architecture: `linux/arm64` on an
ARM64 host or `linux/amd64` on an AMD64 host. Cross-architecture builds are rejected. An optional
platform assertion can make CI fail if a runner has the wrong architecture:

```bash
PODMESH_PLATFORM=linux/arm64 ./deploy/build_containers.sh
```

Run the rootless stack:

```bash
podman kube play deploy/podmesh_rootless.yml --network podmesh
```

The `scheduler` pod contains:

- `podmesh-scheduler` on host port `3000`, holding only expiring in-memory advertisements.
- `podmesh-agent` on host port `3100`, with the Podman socket and a persistent volume containing
  its node keys and encrypted per-workload records. The default limit is 100 workloads, additionally
  bounded by aggregate CPU, memory, and storage capacity.

The `proxy` pod remains the workload traffic plane. Injected sidecars bootstrap through
`/dns4/proxy/udp/4002/quic-v1`.

Apply and delete a workload:

```bash
cargo build -p podctl
./target/debug/podctl --api-url http://127.0.0.1:3000 apply -f deploy/demo_deployment.yml
./target/debug/podctl --api-url http://127.0.0.1:3000 delete -f deploy/demo_deployment.yml
```

`podctl` stores deployment receipts under `~/.podmesh/workloads/`. Running workloads do not depend
on scheduler persistence. If the sole agent and its durable state are lost, the owner must apply the
workload again.

Run the Podman-gated integration tests only after the socket and images are ready:

```bash
cargo test -p podmesh-integration-tests --features podman-tests
```

The gated tests fail when Podman, its socket, or required images are unavailable; they do not
silently skip those prerequisites.