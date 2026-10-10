#!/bin/sh
set -eu

mount -t tmpfs tmpfs /run
mkdir -p /run/sshd /root/.ssh /config
mount -t ext4 -o ro /dev/vdb /config
. /config/guest.env
hostname "$VM_NAME"
ip link set eth0 up
ip addr add "$VM_ADDRESS/24" dev eth0
ip route add default via "$VM_GATEWAY"
cp /config/authorized_keys /root/.ssh/authorized_keys
chmod 700 /root/.ssh
chmod 600 /root/.ssh/authorized_keys
ssh-keygen -A
printf 'PODMESH_SSH_HOST_KEY='
cat /etc/ssh/ssh_host_ed25519_key.pub
sysctl -w net.ipv4.ip_forward=1
echo "PODMESH_GUEST_READY=$VM_NAME"
