#!/bin/sh
set -eu
role=$1
command="/opt/podmesh/$role-command"
while [ ! -x "$command" ]; do sleep 1; done
echo "$$" > "/run/podmesh-$role.pid"
exec "$command" >> "/var/log/podmesh-$role.log" 2>&1
