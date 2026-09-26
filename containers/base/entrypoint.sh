#!/bin/sh
set -eu

# Runs as root under tini on every bottle start. Keep it fast.

# Generate an SSH key on first boot only.
[ -f /etc/ssh/ssh_host_ed25519_key ] || ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key

# Start SSH in the foreground.
mkdir -p /run/sshd
exec /usr/sbin/sshd -D -e
