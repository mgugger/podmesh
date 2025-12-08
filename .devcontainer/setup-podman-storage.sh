#!/usr/bin/env bash
set -e

mkdir -p /tmp/containers/{runroot,storage}

cat > /etc/containers/storage.conf <<'EOF'
[storage]
driver = "overlay"
runroot = "/tmp/containers/runroot"
graphroot = "/tmp/containers/storage"

[storage.options.overlay]
mount_program = "/usr/bin/fuse-overlayfs"
EOF

mkdir /run/podman
podman system service --time=0 unix:///run/podman/podman.sock &

mkdir -p /run/user/1000/podman
podman system service --time=0 unix:///run/user/1000/podman/podman.sock &