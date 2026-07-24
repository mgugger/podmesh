#!/usr/bin/env sh
set -eu

case "$(uname -m)" in
	x86_64) NATIVE_PLATFORM=linux/amd64 ;;
	aarch64 | arm64) NATIVE_PLATFORM=linux/arm64 ;;
	*)
		echo "unsupported native architecture: $(uname -m)" >&2
		exit 1
		;;
esac

PLATFORM=${PODMESH_PLATFORM:-$NATIVE_PLATFORM}
if [ "$PLATFORM" != "$NATIVE_PLATFORM" ]; then
	echo "cross-architecture builds are unsupported: host=$NATIVE_PLATFORM requested=$PLATFORM" >&2
	exit 1
fi

build_image() {
	image=$1
	dockerfile=$2

	podman manifest rm "$image" >/dev/null 2>&1 || true
	podman image rm --force "$image" >/dev/null 2>&1 || true
	podman build \
		--platform "$PLATFORM" \
		--manifest "$image" \
		-f "$dockerfile" \
		.
}

build_image podmesh/scheduler:latest podmesh-scheduler/Dockerfile
build_image podmesh/agent:latest podmesh-agent/Dockerfile
build_image podmesh/proxy:latest podmesh-proxy/Dockerfile
build_image podmesh/sidecar:latest podmesh-sidecar/Dockerfile