podman build -t podmesh/scheduler -f podmesh-scheduler/Dockerfile .
podman build -t podmesh/proxy -f podmesh-proxy/Dockerfile .
podman build -t podmesh/sidecar -f podmesh-sidecar/Dockerfile .