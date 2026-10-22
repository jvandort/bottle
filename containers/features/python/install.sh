#!/bin/sh
set -eu

apt-get update
apt-get install -y --no-install-recommends \
  python-is-python3 \
  python3 \
  python3-pip \
  python3-venv
rm -rf /var/lib/apt/lists/*

# PEP 668: Debian marks the system Python "externally managed", so pip refuses
# to install into it at all -- not system-wide, not even --user -- until you
# make a virtualenv or pass --break-system-packages. Right for a machine you
# keep, wrong for a bottle, which exists so an agent can work without asking
# and which a new bottle replaces in a second. externallyManaged keeps it.
if [ "$EXTERNALLYMANAGED" = "true" ]; then
  echo "python: keeping Debian's PEP 668 marker; pip installs need a virtualenv"
else
  rm -f /usr/lib/python3*/EXTERNALLY-MANAGED
fi

# Without sudo, pip puts console scripts in ~/.local/bin. Debian's ~/.profile
# puts that on PATH, but only if it exists when a login shell starts.
install -d -o "$_REMOTE_USER" -g "$_REMOTE_USER" "$_REMOTE_USER_HOME/.local/bin"
