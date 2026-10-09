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

IMAGE_TAG=${PODMESH_IMAGE_TAG:-latest}
case "$IMAGE_TAG" in
	'' | *[!A-Za-z0-9_.-]* | .* | *.)
		echo "invalid PODMESH_IMAGE_TAG: $IMAGE_TAG" >&2
		exit 1
		;;
esac
if [ "${#IMAGE_TAG}" -gt 128 ]; then
	echo "PODMESH_IMAGE_TAG exceeds 128 characters" >&2
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
	image_id=$(podman image inspect --format '{{.Id}}' "$image")
	printf '%s\n' "built $image=$image_id"
}

build_image "podmesh/scheduler:$IMAGE_TAG" scheduler
build_image "podmesh/agent:$IMAGE_TAG" agent
build_image "podmesh/proxy:$IMAGE_TAG" proxy
build_image "podmesh/sidecar:$IMAGE_TAG" sidecar