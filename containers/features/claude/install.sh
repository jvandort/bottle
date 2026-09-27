#!/bin/sh
set -eu

# The repository is https; apt needs root certificates to reach it.
apt-get update
apt-get install -y --no-install-recommends ca-certificates

# The signing key is vendored (fingerprint 31DD DE24 DDFA B679 F42D 7BD2 BAA9
# 29FF 1A7E CACE, per the Claude Code setup docs) so builds don't trust a key
# fetched at build time.
install -D -m 0644 claude-code.asc /etc/apt/keyrings/claude-code.asc
echo "deb [signed-by=/etc/apt/keyrings/claude-code.asc] https://downloads.claude.ai/claude-code/apt/stable stable main" \
  > /etc/apt/sources.list.d/claude-code.list

apt-get update
apt-get install -y --no-install-recommends claude-code
rm -rf /var/lib/apt/lists/*
