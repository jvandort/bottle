#!/bin/sh
set -eu

# VERSION and ADDITIONALVERSIONS come from the feature's options.
versions="$VERSION $(echo "$ADDITIONALVERSIONS" | tr ',' ' ')"
for v in $versions; do
  case "$v" in
    *[!0-9]*) echo "jvm: versions must be JDK feature releases like 21, not '$v'" >&2; exit 1 ;;
  esac
done

# The repository is https; apt needs root certificates to reach it.
apt-get update
apt-get install -y --no-install-recommends ca-certificates

# The signing key is vendored (fingerprint 3B04 D753 C905 0D9A 5D34 3F39 843C
# 48A5 65F8 F04B, as served by Adoptium and keyserver.ubuntu.com) so builds
# don't trust a key fetched at build time.
install -D -m 0644 adoptium.asc /etc/apt/keyrings/adoptium.asc
# In a subshell: os-release defines its own VERSION, which would clobber ours.
codename=$(. /etc/os-release && echo "$VERSION_CODENAME")
echo "deb [signed-by=/etc/apt/keyrings/adoptium.asc] https://packages.adoptium.net/artifactory/deb $codename main" \
  > /etc/apt/sources.list.d/adoptium.list

apt-get update
packages=""
for v in $versions; do
  if ! apt-cache show "temurin-$v-jdk" >/dev/null 2>&1; then
    available=$(apt-cache pkgnames temurin- | sed -n 's/^temurin-\([0-9]*\)-jdk$/\1/p' | sort -n | tr '\n' ' ')
    echo "jvm: Adoptium has no JDK $v for this system; available: $available" >&2
    exit 1
  fi
  packages="$packages temurin-$v-jdk"
done
# shellcheck disable=SC2086 # word splitting is intended
apt-get install -y --no-install-recommends $packages
rm -rf /var/lib/apt/lists/*

arch=$(dpkg --print-architecture)
# A stable path per version (/usr/lib/jvm/jdk-17). Everything lives in /usr/lib/jvm.
for v in $versions; do
  ln -sfn "/usr/lib/jvm/temurin-$v-jdk-$arch" "/usr/lib/jvm/jdk-$v"
done

# The primary version is JAVA_HOME (see containerEnv) and the system's java,
# whichever version the packages' alternatives would otherwise have picked.
primary="/usr/lib/jvm/temurin-$VERSION-jdk-$arch"
ln -sfn "$primary" /usr/lib/jvm/default-jdk
for tool in "$primary"/bin/*; do
  name=$(basename "$tool")
  if update-alternatives --list "$name" >/dev/null 2>&1; then
    update-alternatives --set "$name" "$tool" >/dev/null
  fi
done
