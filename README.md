# **Podmesh**

A scheduler for pods built with libp2p. 

* podmesh-scheduler is the podman scheduler
* podmesh-proxy is the ingress / egress node
* podmesh-sidecar is the sidecar in the pod that connects to the proxy

* See deploy/README.md for usage and local deployment

## Testing

### Running Tests

Run all tests (excluding podman-dependent tests):
```bash
cargo test
```

Run all tests including podman-dependent integration tests:
```bash
cargo test --features podman-tests
```

Run only podman-dependent tests in the integration test crate:
```bash
cargo test --package podmesh-integration-tests --features podman-tests
```

Run only podman-dependent tests in the scheduler crate:
```bash
cargo test --package podmesh-scheduler --features podman-tests
```

Run all podman tests across all crates:
```bash
cargo test --workspace --features podman-tests
```

**Prerequisites for podman tests:**
- Podman must be installed and available in PATH
- Rootless podman socket must be running: `systemctl --user start podman.socket`
- Required container images must be built: `./deploy/build_containers.sh`

**License**: Apache 2.0
