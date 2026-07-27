# Podman Deployment

Create the workload network and make the Podman socket available:

```bash
systemctl --user start podman.socket
podman network create podmesh
./deploy/build_containers.sh
```

The build uses `deploy/Containerfile` to compile all four Rust binaries in one shared musl builder
stage, then assembles four scratch runtime targets. Shared dependencies are compiled once and later
image targets reuse the cached binary layer. Images are created only for the native host
architecture: `linux/arm64` on an ARM64 host or `linux/amd64` on an AMD64 host.
Cross-architecture builds are rejected. An optional platform assertion can make CI fail if a runner
has the wrong architecture:

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

The `proxy` pod remains the workload traffic plane. Every logical regional proxy has a persistent
key directory and therefore a stable libp2p peer ID. Initialize each identity before creating the
pod topology:

```bash
podman volume create podmesh-proxy-state
podman run --rm -e RUST_LOG=info -v podmesh-proxy-state:/var/lib/podmesh-proxy \
  podmesh/proxy:latest --init-identity --key-dir /var/lib/podmesh-proxy/proxy-bootstrap
podman run --rm -e RUST_LOG=info -v podmesh-proxy-state:/var/lib/podmesh-proxy \
  podmesh/proxy:latest --init-identity --key-dir /var/lib/podmesh-proxy/proxy-1
podman run --rm -e RUST_LOG=info -v podmesh-proxy-state:/var/lib/podmesh-proxy \
  podmesh/proxy:latest --init-identity --key-dir /var/lib/podmesh-proxy/proxy-2
```

Record the logged peer IDs and construct full reachable multiaddrs ending in
`/p2p/<peer-id>`. Before `podman kube play`, provision the `podmesh-proxy-topology` Secret required
by the deployment manifest. Both auxiliary proxies use its `bootstrap-peer` value to join the
bootstrap proxy:

```bash
cat >/tmp/podmesh-proxy-topology.yml <<EOF
apiVersion: v1
kind: Secret
metadata:
  name: podmesh-proxy-topology
stringData:
  bootstrap-peer: /dns4/proxy/udp/4002/quic-v1/p2p/<proxy-bootstrap-peer-id>
EOF
podman secret create --replace podmesh-proxy-topology /tmp/podmesh-proxy-topology.yml
```

The secret is proxy mesh topology, not an authorization root. Each tenant supplies its own initial
sidecar proxy list in the owner-controlled workload manifest:

```yaml
metadata:
  annotations:
    podmesh.io/proxy-peers: >-
      /ip4/203.0.113.10/udp/4002/quic-v1/p2p/12D3...,
      /ip4/198.51.100.20/udp/4003/quic-v1/p2p/12D3...,
      /ip4/192.0.2.30/udp/4004/quic-v1/p2p/12D3...
```

Alternatively, set `PODMESH_PROXY_PEERS` in the environment running `podctl apply`. `podctl`
validates and embeds these records in the encrypted, owner-signed execution specification; the
selected agent and scheduler do not choose tenant proxies. Sidecars verify every configured or
discovered proxy against the tenant-issued `NodeCert` before registration or traffic. Back up the
proxy identity volume and ensure only one active process uses each key directory.

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