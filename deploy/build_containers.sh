podman build -t podmesh/machine -f podmesh-scheduler/Dockerfile .
podman build -t podmesh/workload -f podmesh-proxy/Dockerfile .
podman build -t podmesh/sidecar -f podmesh-sidecar/Dockerfile .