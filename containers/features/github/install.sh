#!/bin/sh
set -eu

# The repository is https; apt needs root certificates to reach it.
apt-get update
apt-get install -y --no-install-recommends ca-certificates

# The signing keys are vendored (fingerprints 2C61 0620 1985 B60E 6C7A C873
# 23F3 D4EA 7571 6059 and its 2026 successor 7F38 BBB5 9D06 4DBC B3D8 4D72
# 5612 B364 6231 3325, per GitHub's install instructions for Debian) so builds
# don't trust a key fetched at build time.
install -D -m 0644 githubcli-archive-keyring.gpg /etc/apt/keyrings/githubcli-archive-keyring.gpg
echo "deb [signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
  > /etc/apt/sources.list.d/github-cli.list

apt-get update
apt-get install -y --no-install-recommends gh
rm -rf /var/lib/apt/lists/*

# How this feature is authenticated, since JSON has no room for a comment:
#
#   gh and git talk to github.com over https as they always do. The egress
#   proxy terminates that TLS with a certificate from the egress CA, which
#   the bottle trusts, and attaches the token to requests for github.com,
#   api.github.com and uploads.github.com (see bottle/egress.py).
#
#   GH_TOKEN is the credential's inject.standin, so bottle puts a fake token
#   there when it creates a bottle, and gh believes it's logged in. git needs
#   no stand-in: it sends its request without credentials, and the proxy adds
#   them.
#
# What the token can do is what a bottle can do on GitHub, which is why the
# credential asks for a fine-grained token: pushes that reach GitHub directly
# don't pass through your repo the way a bottle's lane does.

# git over ssh has no key in a bottle, and the proxy can't attach anything to
# it, so GitHub's ssh remotes are fetched and pushed over https instead. With
# --system, since the tools feature (installed first) owns /etc/gitconfig.
git config --system url."https://github.com/".insteadOf "git@github.com:"
git config --system --add url."https://github.com/".insteadOf "ssh://git@github.com/"

# gh's settings for the bottle's user: https for the repos it clones, and no telemetry
home="$_REMOTE_USER_HOME"
install -d -o "$_REMOTE_USER" -g "$_REMOTE_USER" "$home/.config" "$home/.config/gh"
cat > "$home/.config/gh/config.yml" <<YAML
git_protocol: https
telemetry: disabled
YAML
chown "$_REMOTE_USER:$_REMOTE_USER" "$home/.config/gh/config.yml"
