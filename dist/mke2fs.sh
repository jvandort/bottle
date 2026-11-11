#!/usr/bin/env bash
# Builds the mke2fs bottle ships: e2fsprogs' own, from its release tarball, with tar input
# (`mke2fs -d -`) from libarchive linked in. It needs nothing installed where it runs: on Linux
# it's static (musl), on macOS it links only libSystem, which every Mac has.
#
#     dist/mke2fs.sh OUT_DIR
#
# leaves OUT_DIR/mke2fs. bottle runs it as a program of its own and never links it: e2fsprogs'
# library is GPL-2 in parts, bottle is Apache-2.0.
#
# Needs curl, make and a C compiler; on Linux, musl-gcc (Debian's musl-tools) and the kernel's
# headers (linux-libc-dev). Downloads go to $BOTTLE_DIST_CACHE (default ~/.cache/bottle-dist),
# each checked against the sha256 below.

set -euo pipefail

E2FSPROGS=1.47.4
E2FSPROGS_URL=https://www.kernel.org/pub/linux/kernel/people/tytso/e2fsprogs/v$E2FSPROGS/e2fsprogs-$E2FSPROGS.tar.xz
# From the signed sha256sums.asc beside it.
E2FSPROGS_SHA256=fd5bf388cbdbe006a3d3b318d983b2948382440acc85a87f1e7d108653e8db0b

LIBARCHIVE=3.8.10
LIBARCHIVE_URL=https://github.com/libarchive/libarchive/releases/download/v$LIBARCHIVE/libarchive-$LIBARCHIVE.tar.xz
LIBARCHIVE_SHA256=56cad765459d5ee25a3b761f90c47d99379d28297fbfb72d908880d6b875fb96

out=${1:?usage: dist/mke2fs.sh OUT_DIR}
mkdir -p "$out"
out=$(cd "$out" && pwd)
cache=${BOTTLE_DIST_CACHE:-$HOME/.cache/bottle-dist}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
jobs=$(getconf _NPROCESSORS_ONLN)

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

# The tarball at $1, checked against $2, unpacked into $work.
fetch() {
    local url=$1 want=$2 file
    file=$cache/$(basename "$url")
    if [ ! -f "$file" ] || [ "$(sha256 "$file")" != "$want" ]; then
        mkdir -p "$cache"
        curl -fsSL -o "$file.part" "$url"
        mv "$file.part" "$file"
    fi
    if [ "$(sha256 "$file")" != "$want" ]; then
        echo "mke2fs.sh: $url doesn't have the sha256 expected" >&2
        exit 1
    fi
    tar -xf "$file" -C "$work"
}

case $(uname -s) in
    Linux)
        export CC=musl-gcc
        static=-static
        # musl brings no kernel headers; llseek.c and others want linux/*.h and asm/*.h.
        mkdir "$work/kernel-headers"
        ln -s /usr/include/linux /usr/include/asm-generic "$work/kernel-headers/"
        ln -s "/usr/include/$(gcc -dumpmachine)/asm" "$work/kernel-headers/asm"
        export CPPFLAGS="-idirafter $work/kernel-headers"
        ;;
    Darwin)
        export CC=${CC:-cc}
        # Apple supports no static executables; libSystem stays dynamic, all else is linked in.
        static=
        ;;
    *)
        echo "mke2fs.sh: no build for $(uname -s)" >&2
        exit 1
        ;;
esac

log=$work/build.log
step() {
    echo "mke2fs.sh: $1" >&2
    shift
    if ! "$@" >>"$log" 2>&1; then
        tail -30 "$log" >&2
        exit 1
    fi
}

# libarchive for reading tar alone: no compression, no crypto, no character set conversion.
fetch "$LIBARCHIVE_URL" "$LIBARCHIVE_SHA256"
prefix=$work/prefix
(
    cd "$work/libarchive-$LIBARCHIVE"
    step "configuring libarchive $LIBARCHIVE" ./configure --prefix="$prefix" \
        --disable-shared --enable-static --disable-bsdtar --disable-bsdcpio --disable-bsdcat \
        --disable-bsdunzip --disable-acl --without-zlib --without-bz2lib --without-libb2 \
        --without-iconv --without-lz4 --without-zstd --without-lzma --without-cng \
        --without-openssl --without-xml2 --without-expat --without-mbedtls --without-nettle
    step "building libarchive" make -j"$jobs"
    step "installing libarchive" make install
)

# configure asks pkg-config, and only pkg-config, where libarchive is (and insists there is a
# pkg-config at all, which Xcode lacks): this one answers for ours.
cat >"$work/pkg-config" <<EOF
#!/bin/sh
case "\$*" in
    *--cflags*) echo "-I$prefix/include" ;;
    *--libs*) echo "-L$prefix/lib -larchive" ;;
esac
EOF
chmod +x "$work/pkg-config"

fetch "$E2FSPROGS_URL" "$E2FSPROGS_SHA256"
mkdir "$work/e2fsprogs-build"
(
    cd "$work/e2fsprogs-build"
    # --enable-libuuid and --enable-libblkid build e2fsprogs' own; --disable would mean the system's.
    step "configuring e2fsprogs $E2FSPROGS" env PKG_CONFIG="$work/pkg-config" LDFLAGS="$static" \
        "../e2fsprogs-$E2FSPROGS/configure" --with-libarchive=direct --enable-libuuid \
        --enable-libblkid --disable-nls --disable-fuse2fs --disable-e2initrd-helper \
        --disable-debugfs --disable-imager --disable-resizer --disable-defrag --disable-uuidd \
        --disable-tdb --disable-bmap-stats --disable-tls --disable-backtrace --without-pthread
    if ! grep -q '^#define HAVE_ARCHIVE_H 1' lib/config.h; then
        echo "mke2fs.sh: e2fsprogs' configure didn't find libarchive" >&2
        exit 1
    fi
    # `libs` builds the libraries in order, each in parallel: one after another, each
    # library's headers are there before the next needs them.
    step "building e2fsprogs' libraries" make -j"$jobs" libs
    step "building mke2fs" make -j"$jobs" -C misc mke2fs
    cp misc/mke2fs "$out/mke2fs"
)
echo "mke2fs.sh: $out/mke2fs" >&2
