#!/bin/sh
set -eu

# The JDKs, from Adoptium's apt repository, are its customizations.bottle.apt's
# (temurin-<version>-jdk for VERSION and each of ADDITIONALVERSIONS, the
# feature's options), installed before this runs; a version Adoptium hasn't
# got fails there. The repository's signing key is vendored, adoptium.asc
# (fingerprint 3B04 D753 C905 0D9A 5D34 3F39 843C 48A5 65F8 F04B, as served by
# Adoptium and keyserver.ubuntu.com), so builds don't trust a key fetched at
# build time. Its suite is Debian's release, trixie, as images.DEBIAN is.
versions="$VERSION $(echo "$ADDITIONALVERSIONS" | tr ',' ' ')"

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
