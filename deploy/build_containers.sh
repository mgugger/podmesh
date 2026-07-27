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
	target=$2

	podman manifest rm "$image" >/dev/null 2>&1 || true
	podman image rm --force "$image" >/dev/null 2>&1 || true
	podman build \
		--platform "$PLATFORM" \
		--layers \
		--tag "$image" \
		--target "$target" \
		-f deploy/Containerfile \
		.
}

build_image podmesh/scheduler:latest scheduler
build_image podmesh/agent:latest agent
build_image podmesh/proxy:latest proxy
build_image podmesh/sidecar:latest sidecar