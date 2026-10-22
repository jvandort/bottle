#!/bin/sh
set -eu

# Runs under tini on every bottle start. Keep it fast.

# sshd setup needs root; skip it when the container runs as another user (`run --user`).
if [ "$(id -u)" = 0 ]; then
  # Generate an SSH key on first boot only.
  [ -f /etc/ssh/ssh_host_ed25519_key ] || ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key
  mkdir -p /run/sshd

  # bottle sets some variables as the machine's env; SSH sessions only see
  # /etc/environment, so mirror there the ones BOTTLE_MIRROR_ENV names,
  # currently the stand-in tokens a feature asked for (see bottle/auth.py).
  # Rewritten on every start.
  for var in ${BOTTLE_MIRROR_ENV:-}; do
    sed -i "/^$var=/d" /etc/environment
    eval "value=\${$var:-}"
    [ -z "$value" ] || echo "$var=$value" >> /etc/environment
  done
fi

# Run the command: sshd by default (see CMD), or whatever was passed to `run`.
exec "$@"
