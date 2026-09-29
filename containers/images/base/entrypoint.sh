#!/bin/sh
set -eu

# Runs under tini on every bottle start. Keep it fast.

# sshd setup needs root; skip it when the container runs as another user (`run --user`).
if [ "$(id -u)" = 0 ]; then
  # Generate an SSH key on first boot only.
  [ -f /etc/ssh/ssh_host_ed25519_key ] || ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key
  mkdir -p /run/sshd

  # Proxy settings arrive as container env (`run --env`); SSH sessions only see
  # /etc/environment, so mirror them there. Rewritten on every start.
  # BOTTLE_MIRROR_ENV names anything else bottle set that way, currently the
  # stand-in tokens a feature asked for (see bottle/auth.py).
  for var in http_proxy https_proxy no_proxy HTTP_PROXY HTTPS_PROXY NO_PROXY ${BOTTLE_MIRROR_ENV:-}; do
    sed -i "/^$var=/d" /etc/environment
    eval "value=\${$var:-}"
    [ -z "$value" ] || echo "$var=$value" >> /etc/environment
  done
fi

# Run the command: sshd by default (see CMD), or whatever was passed to `run`.
exec "$@"
