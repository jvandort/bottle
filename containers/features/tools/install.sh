#!/bin/sh
set -eu

# ca-certificates: TLS root certificates; without them curl, git, and apt over https fail.
# openssh-client: git over ssh. procps: ps, top, kill.
apt-get update
apt-get install -y --no-install-recommends \
  ca-certificates \
  curl \
  fd-find \
  jq \
  less \
  openssh-client \
  procps \
  ripgrep \
  tmux \
  unzip \
  vim \
  xz-utils
rm -rf /var/lib/apt/lists/*

# Debian names the binary fdfind; expose it under its upstream name.
ln -sf /usr/bin/fdfind /usr/local/bin/fd

install -m 0644 gitconfig /etc/gitconfig
install -m 0644 tmux.conf /etc/tmux.conf

# shpool, which folio runs each session in: built from source, for a fix its
# release binary hasn't got yet (see shpool-build.sh). No apt repository has it.
sh shpool-build.sh /usr/local/bin
install -d /etc/shpool
install -m 0644 shpool.toml /etc/shpool/config.toml
