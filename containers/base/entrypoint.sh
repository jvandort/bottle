#!/bin/sh
set -eu

# Runs under tini on every bottle start. Keep it fast.

# sshd setup needs root; skip it when the container runs as another user (`run --user`).
if [ "$(id -u)" = 0 ]; then
  # Generate an SSH key on first boot only.
  [ -f /etc/ssh/ssh_host_ed25519_key ] || ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key
  mkdir -p /run/sshd
fi

# Run the command: sshd by default (see CMD), or whatever was passed to `run`.
exec "$@"
