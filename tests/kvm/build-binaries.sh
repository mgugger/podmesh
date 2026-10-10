#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$root"
[ "$(uname -m)" = x86_64 ] || { echo "the initial KVM slice requires x86_64" >&2; exit 1; }
target=x86_64-unknown-linux-musl
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc
export RUSTFLAGS="-C relocation-model=static -C link-arg=-no-pie"
cargo build --locked --target "$target" \
    -p podmesh-scheduler -p podmesh-agent -p podmesh-proxy -p podmesh-sidecar
mkdir -p target/kvm/bin
for role in scheduler agent proxy sidecar; do
    cp "target/$target/debug/podmesh-$role" "target/kvm/bin/podmesh-$role.new"
    strip --strip-debug "target/kvm/bin/podmesh-$role.new"
    mv "target/kvm/bin/podmesh-$role.new" "target/kvm/bin/podmesh-$role"
done
rustc --edition=2024 --target "$target" -C linker=musl-gcc \
    -C relocation-model=static -C link-arg=-no-pie \
    tests/kvm/workload-http.rs -o target/kvm/bin/podmesh-kvm-http.new
strip --strip-debug target/kvm/bin/podmesh-kvm-http.new
mv target/kvm/bin/podmesh-kvm-http.new target/kvm/bin/podmesh-kvm-http
