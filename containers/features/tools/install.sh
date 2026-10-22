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

# shpool, which folio runs each session in: from its GitHub release, checked
# against the checksums kept here (no apt repository has it). The glibc build,
# linked against Debian's libc (it needs 2.34 or later), so Debian's libc
# updates reach it; the musl one is static, with its libc built in.
version=0.11.5
case "$(dpkg --print-architecture)" in
  arm64) target=aarch64-unknown-linux-gnu ;;
  amd64) target=x86_64-unknown-linux-gnu ;;
  *) echo "tools: shpool has no release for $(dpkg --print-architecture)" >&2; exit 1 ;;
esac
archive="shpool-$target.tar.gz"
cp "shpool-$version-checksums.txt" /tmp/
cd /tmp
curl -fsSL -o "$archive" "https://github.com/shell-pool/shpool/releases/download/v$version/$archive"
grep " $archive\$" "shpool-$version-checksums.txt" | sha256sum -c -
tar -xzf "$archive" shpool
install -m 0755 shpool /usr/local/bin/shpool
rm -f "$archive" shpool "shpool-$version-checksums.txt"
cd "$OLDPWD"
install -d /etc/shpool
install -m 0644 shpool.toml /etc/shpool/config.toml
