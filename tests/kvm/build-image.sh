#!/bin/sh
set -eu

# Build dependencies stay in this private rootfs, never in the finished guest.
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
out="$root/target/kvm"
version=3.23.6
checksum=6fc0e3639a1c01f156970d7626aab90bc90697117069dbc39ad880b84efa319a

[ "$(uname -m)" = x86_64 ] || { echo "the initial KVM slice requires x86_64" >&2; exit 1; }
[ "$(id -u)" = 0 ] || { echo "image construction requires root for chroot" >&2; exit 1; }
for tool in curl sha256sum tar chroot mkfs.ext4 truncate; do
    command -v "$tool" >/dev/null || { echo "missing image builder: $tool" >&2; exit 1; }
done
mkdir -p "$out"
fingerprint=$(sha256sum "$root/tests/kvm/build-image.sh" \
    "$root/tests/kvm/guest-boot.sh" "$root/tests/kvm/guest-role.sh" | sha256sum | cut -d ' ' -f 1)
if [ -e "$out/agent.raw" ] && [ -e "$out/image.sha256" ] && \
    [ "$(cat "$out/image.sha256")" = "$fingerprint" ]; then
    echo "cached image exists: $out/agent.raw"
    exit 0
fi
stage=$(mktemp -d "$out/build.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT HUP INT TERM
curl -fsSL --max-time 120 \
    "https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/x86_64/alpine-minirootfs-$version-x86_64.tar.gz" \
    -o "$stage/rootfs.tar.gz"
echo "$checksum  $stage/rootfs.tar.gz" | sha256sum -c -
mkdir "$stage/root"
tar -xzf "$stage/rootfs.tar.gz" -C "$stage/root"
cp /etc/resolv.conf "$stage/root/etc/resolv.conf"
chroot "$stage/root" /sbin/apk add --no-cache \
    ca-certificates openssh-server openssh-sftp-server iproute2 \
    podman netavark aardvark-dns crun conmon nftables curl
chroot "$stage/root" /sbin/apk info -v | sort > "$stage/packages.txt"
mkdir -p "$stage/root/etc/containers" "$stage/root/usr/local/libexec" \
    "$stage/root/config" "$stage/root/opt/podmesh"
cp "$root/tests/kvm/guest-boot.sh" "$stage/root/usr/local/libexec/guest-boot"
cp "$root/tests/kvm/guest-role.sh" "$stage/root/usr/local/libexec/guest-role"
chmod 755 "$stage/root/usr/local/libexec/guest-boot" "$stage/root/usr/local/libexec/guest-role"
printf '%s\n' \
    '::sysinit:/usr/local/libexec/guest-boot' \
    '::respawn:/usr/sbin/sshd -D -e' \
    '::respawn:/usr/local/libexec/guest-role scheduler' \
    '::respawn:/usr/local/libexec/guest-role proxy' \
    '::respawn:/usr/local/libexec/guest-role agent' \
    '::shutdown:/bin/sync' > "$stage/root/etc/inittab"
cat > "$stage/root/etc/ssh/sshd_config" <<'EOF'
PermitRootLogin prohibit-password
PasswordAuthentication no
KbdInteractiveAuthentication no
LogLevel VERBOSE
Subsystem sftp internal-sftp
EOF
cat > "$stage/root/etc/containers/containers.conf" <<'EOF'
[engine]
cgroup_manager = "cgroupfs"
events_logger = "file"
[network]
network_backend = "netavark"
firewall_driver = "nftables"
EOF
cat > "$stage/root/etc/containers/storage.conf" <<'EOF'
[storage]
driver = "overlay"
runroot = "/run/containers/storage"
graphroot = "/var/lib/containers/storage"
EOF
# Locked root passwords prevent public-key SSH on some OpenSSH configurations.
# Disable passwords in sshd instead; the only authorized key is injected per run.
sed -i 's/^root:[^:]*:/root::/' "$stage/root/etc/shadow"
truncate -s 4G "$stage/agent.raw"
mkfs.ext4 -q -F -d "$stage/root" "$stage/agent.raw"
mv "$stage/packages.txt" "$out/packages.txt"
mv "$stage/agent.raw" "$out/agent.raw"
printf '%s\n' "$fingerprint" > "$out/image.sha256"
echo "image: $out/agent.raw"
du -h "$out/agent.raw"
