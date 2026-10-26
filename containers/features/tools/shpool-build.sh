#!/bin/sh
set -eu

# Builds shpool from source and installs it in the directory $1, leaving
# nothing else behind: the Rust toolchain and the build are in a directory of
# their own, removed when done, and a C compiler this installs (rustc links with
# it) is removed again, with whatever it pulled in.
#
# Why not the release binary: shpool 0.11.5's is built with shpool-vterm 0.2.2,
# whose copy of a session's screen (session_restore_engine = "vterm", see
# shpool.toml) panics on any zero-width character -- a combining accent, or the
# variation selector of an emoji such as those Claude Code suggests when `:` is
# typed. The panic takes the session's output thread with it: the terminal is
# dropped, and every attach after hangs or fails at once, while the program
# carries on unseen. shpool-vterm fixed it in 0.2.5 ("zero width char panic"
# in its changelog), with other panics, and libshpool 0.11.5 takes any 0.2
# (it asks for ^0.2.2): the release's own lock file is used, with only
# shpool-vterm updated, to 0.2.8, the latest.
#
# Everything is checked: the source, rustup-init and shpool-vterm against
# shpool-0.11.5-checksums.txt, every other crate against the release's lock
# file (--locked), and the toolchain by rustup, against its release's manifest.
version=0.11.5
vterm_version=0.2.8
rustup_version=1.29.2
toolchain=1.99.1
dest=$1
here=$(cd "$(dirname "$0")" && pwd)

# Stops at the first shpool release after this one: it may be built with the
# fix, and then this build should go, for the release binary again (as this
# feature installed it before: see its history). If it isn't, take its source
# and lock file here instead, with shpool-vterm updated the same way.
latest=$(curl -fsSL -A "bottle tools feature" https://crates.io/api/v1/crates/shpool | jq -r .crate.max_stable_version)
if [ "$latest" != "$version" ]; then
  cat >&2 <<EOF
tools: shpool $latest is out, and this feature builds $version from source only
for the fix in shpool-vterm 0.2.8 (see containers/features/tools/shpool-build.sh).
If shpool $latest's Cargo.lock has shpool-vterm 0.2.8 or later, install its
release binary instead, and delete shpool-build.sh and its files; if not, build
$latest here the same way.
EOF
  exit 1
fi

case "$(dpkg --print-architecture)" in
  arm64) target=aarch64-unknown-linux-gnu ;;
  amd64) target=x86_64-unknown-linux-gnu ;;
  *) echo "tools: no Rust toolchain to build shpool with for $(dpkg --print-architecture)" >&2; exit 1 ;;
esac

work=$(mktemp -d)
# The compiler, unless something installed before this has it already: only
# what this adds is removed after.
added=""
for package in gcc libc6-dev; do
  dpkg-query -W -f '${Status}' "$package" 2>/dev/null | grep -q "ok installed" || added="$added $package"
done
cleanup() {
  rm -rf "$work"
  if [ -n "$added" ]; then
    apt-get purge -y --auto-remove $added
    rm -rf /var/lib/apt/lists/*
  fi
}
trap cleanup EXIT

if [ -n "$added" ]; then
  apt-get update
  apt-get install -y --no-install-recommends $added
fi

# `download URL NAME`: the file at URL, as $work/NAME, if it matches NAME's checksum.
download() {
  curl -fsSL -o "$work/$2" "$1"
  sum=$(grep " $2\$" "$here/shpool-$version-checksums.txt" | cut -d' ' -f1)
  echo "${sum:?no checksum for $2}  $work/$2" | sha256sum -c -
}

download "https://static.rust-lang.org/rustup/archive/$rustup_version/$target/rustup-init" "rustup-init-$rustup_version-$target"
chmod +x "$work/rustup-init-$rustup_version-$target"
export RUSTUP_HOME="$work/rustup" CARGO_HOME="$work/cargo"
"$work/rustup-init-$rustup_version-$target" -y --quiet --no-modify-path --profile minimal --default-toolchain "$toolchain"

download "https://static.crates.io/crates/shpool/shpool-$version.crate" "shpool-$version.crate"
tar -xzf "$work/shpool-$version.crate" -C "$work"
manifest="$work/shpool-$version/Cargo.toml"
lock="$work/shpool-$version/Cargo.lock"
packages() { awk '/^name = /{name=$3} /^version = /{print name, $3}' "$lock" | sort; }
packages > "$work/before"
"$CARGO_HOME/bin/cargo" update --manifest-path "$manifest" -p shpool-vterm --precise "$vterm_version"
packages | diff "$work/before" - | grep '^[<>]' | grep -v '"shpool-vterm"' && {
  echo "tools: updating shpool-vterm changed other packages too (above)" >&2
  exit 1
}
sum=$(grep " shpool-vterm-$vterm_version\$" "$here/shpool-$version-checksums.txt" | cut -d' ' -f1)
grep -A3 '^name = "shpool-vterm"$' "$lock" | grep -qx "checksum = \"${sum:?no checksum for shpool-vterm}\"" || {
  echo "tools: shpool-vterm $vterm_version's checksum isn't the one kept here" >&2
  exit 1
}
# A few jobs, not one per CPU: an image is built in a machine with smolvm's default memory.
"$CARGO_HOME/bin/cargo" build --release --locked --jobs 4 --manifest-path "$manifest"
install -m 0755 "$work/shpool-$version/target/release/shpool" "$dest/shpool"
