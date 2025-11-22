podman build -t podmesh/machine -f machine/Dockerfile .
podman build -t podmesh/workload -f meshproxy/Dockerfile .
podman build -t podmesh/sidecar -f sidecar/Dockerfile .