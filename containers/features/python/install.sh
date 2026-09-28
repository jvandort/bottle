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
# and which `bottle reset` rebuilds in a second. externallyManaged keeps it.
if [ "$EXTERNALLYMANAGED" = "true" ]; then
  echo "python: keeping Debian's PEP 668 marker; pip installs need a virtualenv"
else
  rm -f /usr/lib/python3*/EXTERNALLY-MANAGED
fi

# Without sudo, pip falls back to ~/.local, which isn't on Debian's PATH.
# Login shells get it: `bottle shell` runs bash -l, as does ssh.
install -d -o "$_REMOTE_USER" -g "$_REMOTE_USER" "$_REMOTE_USER_HOME/.local/bin"
cat > /etc/profile.d/python.sh <<'SH'
# Console scripts from `pip install` without sudo land in ~/.local/bin.
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) PATH="$HOME/.local/bin:$PATH"; export PATH ;;
esac
SH
