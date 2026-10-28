#!/bin/sh
# Install the key from $AUTHORIZED_KEY for `dev` and `solo`, then run sshd.
set -eu
for u in dev solo; do
    mkdir -p "/home/$u/.ssh"
    printf '%s\n' "$AUTHORIZED_KEY" > "/home/$u/.ssh/authorized_keys"
    chown -R "$u:$u" "/home/$u/.ssh"
    chmod 700 "/home/$u/.ssh"
    chmod 600 "/home/$u/.ssh/authorized_keys"
done
exec /usr/sbin/sshd -D -e
