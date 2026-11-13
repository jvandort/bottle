#!/bin/sh
set -eu

# Runs under tini on every bottle start (ENTRYPOINT in bottle/images.py). Keep it fast.

# sshd setup needs root; skip it when the container runs as another user (`run --user`).
if [ "$(id -u)" = 0 ]; then
  # Generate an SSH key on first boot only.
  [ -f /etc/ssh/ssh_host_ed25519_key ] || ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key
  mkdir -p /run/sshd

  # The machine's CA, which bottle-cli takes TLS off with for the hosts it caches and routes by name,
  # given at /run/bottle/ca.crt as it boots (none: it takes off none), trusted for them: in the
  # system store, whose bundle NODE_EXTRA_CA_CERTS names too. Rebuilt only when it's changed.
  trusted=/usr/local/share/ca-certificates/bottle-egress-cache.crt
  if [ -f /run/bottle/ca.crt ]; then
    cmp -s /run/bottle/ca.crt "$trusted" || { cp /run/bottle/ca.crt "$trusted" && update-ca-certificates --fresh >/dev/null; }
  elif [ -e "$trusted" ]; then
    rm "$trusted" && update-ca-certificates --fresh >/dev/null
  fi

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

# Run the command: sshd by default (CMD in bottle/images.py), or whatever was passed to `run`.
exec "$@"
