podman build -t beemesh/machine -f machineplane/Dockerfile .
podman build -t beemesh/workload -f workloadplane/workload/Dockerfile .
podman build -t beemesh/gateway -f workloadplane/gateway/Dockerfile .