Ensure that podman.sock is available and create the podman network
```
systemctl start podman.socket
podman network create podmesh
```
Then run
```
podman kube play deploy/complete_rootful.yml --network podmesh
# or rootless
#podman kube play deploy/complete_rootless.yml --network podmesh
```
The machine agent expects an explicit Podman socket argument. Make sure each container spec includes `--podman-socket /run/podman/podman.sock` (or your chosen socket path) so the runtime uses only the mounted socket.

Verify peers
```
curl localhost:3000/debug/dht/peers
```

Build podctl & Apply manifest via podctl
```
cargo build -p podctl
./target/debug/podctl --api-url http://localhost:3000 apply -f deploy/demo_deployment.yml
```

Verify status for a "podmesh-${id}-pod"
```
podman pod ls
```

Delete manifest
```
./target/debug/podctl --api-url http://localhost:3000 delete -f machineplane/tests/sample_manifests/nginx.yml
```