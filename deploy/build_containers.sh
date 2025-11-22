podman build -t podmesh/machine -f machineplane/Dockerfile .
podman build -t podmesh/workload -f meshproxy/Dockerfile .
podman build -t podmesh/gateway -f sidecar/Dockerfile .