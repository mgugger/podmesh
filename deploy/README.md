Ensure that podman.sock is available and create the podman network
```
systemctl start podman.socket
podman network create beemesh
```
Then run
```
podman kube play deploy/complete.yml --network beemesh
```
The machine agent expects an explicit Podman socket argument. Make sure each container spec includes `--podman-socket /run/podman/podman.sock` (or your chosen socket path) so the runtime uses only the mounted socket.

Verify peers
```
curl localhost:3000/debug/dht/peers
```

Build beectl & Apply manifest via beectl
```
cargo build -p beectl
./target/debug/beectl --api-url http://localhost:3000 apply -f machineplane/tests/sample_manifests/nginx.yml
```

Verify status for a "beemesh-${id}-pod"
```
podman pod ls
```

Delete manifest
```
./target/debug/beectl --api-url http://localhost:3000 delete -f machineplane/tests/sample_manifests/nginx.yml
```