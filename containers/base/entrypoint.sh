#!/bin/sh
set -eu

# Runs as root under tini on every bottle start. Keep it fast.

# Generate an SSH key on first boot only.
[ -f /etc/ssh/ssh_host_ed25519_key ] || ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key
mkdir -p /run/sshd

# Run the command: sshd by default (see CMD), or whatever was passed to `run`.
exec "$@"
