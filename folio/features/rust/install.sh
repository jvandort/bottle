#!/bin/sh
set -eu

# rustup-init from rustup's archive, checked against the checksums kept here
# (the ones static.rust-lang.org publishes beside each, recorded when this
# version was chosen), so a build doesn't trust what it downloads.
version=1.29.2
case "$(dpkg --print-architecture)" in
  arm64) target=aarch64-unknown-linux-gnu sha256=a9c168dbea81a84671c6b7a5c1795cb0eb699020110d107ddfca427bd797e697 ;;
  amd64) target=x86_64-unknown-linux-gnu sha256=5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4 ;;
  *) echo "rust: rustup has no build for $(dpkg --print-architecture)" >&2; exit 1 ;;
esac

# curl and ca-certificates are its customizations.bottle.apt's, installed before this runs.
curl -fsSL -o /tmp/rustup-init "https://static.rust-lang.org/rustup/archive/$version/$target/rustup-init"
echo "$sha256  /tmp/rustup-init" | sha256sum -c -
chmod +x /tmp/rustup-init

# Toolchains in RUSTUP_HOME (the feature's containerEnv), shared, rustup and its
# proxies in /usr/local/cargo/bin, linked into /usr/local/bin so every command
# finds them, with no PATH to set: a login shell resets PATH, and another
# feature's would replace it. CARGO_HOME stays each user's ~/.cargo, cargo's
# default: its registry cache, and what `cargo install` builds.
export RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
/tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain "$TOOLCHAIN" \
  $(echo "$COMPONENTS" | tr ',' '\n' | sed '/^$/d; s/^/-c /') \
  $(echo "$TARGETS" | tr ',' '\n' | sed '/^$/d; s/^/-t /')
rm /tmp/rustup-init
for proxy in "$CARGO_HOME"/bin/*; do
  ln -sf "$proxy" "/usr/local/bin/${proxy##*/}"
done

# The bottle's user owns the toolchains, so `rustup target add` and `rustup
# update` need no sudo.
chown -R "$_REMOTE_USER:$_REMOTE_USER" "$RUSTUP_HOME"

# ~/.cargo/bin, where `cargo install` puts programs: on login shells' PATH, as
# rustup's own install would put it.
install -d -o "$_REMOTE_USER" -g "$_REMOTE_USER" "$_REMOTE_USER_HOME/.cargo" "$_REMOTE_USER_HOME/.cargo/bin"
cat > /etc/profile.d/cargo.sh <<'PROFILE'
case ":$PATH:" in *":$HOME/.cargo/bin:"*) ;; *) PATH="$HOME/.cargo/bin:$PATH" ;; esac
PROFILE
