#!/bin/sh
# Install the key from $AUTHORIZED_KEY for `dev`, then run sshd.
set -eu
mkdir -p /home/dev/.ssh
printf '%s\n' "$AUTHORIZED_KEY" > /home/dev/.ssh/authorized_keys
chown -R dev:dev /home/dev/.ssh
chmod 700 /home/dev/.ssh
chmod 600 /home/dev/.ssh/authorized_keys
exec /usr/sbin/sshd -D -e
