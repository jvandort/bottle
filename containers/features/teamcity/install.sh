#!/bin/sh
set -eu

# JetBrains publishes the CLI as a Debian package per release, and no apt
# repository, so the package is fetched from its GitHub release and checked
# against a checksum before it is installed. The default version's checksums
# are in this feature (teamcity-<version>-checksums.txt), so a build doesn't
# trust a file fetched at build time; asking for another version fetches that
# release's checksums over TLS, which is weaker, and deliberate.
[ -n "$SERVER" ] || {
  echo "teamcity: which server? use --feature teamcity:server=https://teamcity.example.com" >&2
  exit 1
}

apt-get update
apt-get install -y --no-install-recommends ca-certificates curl

arch=$(dpkg --print-architecture)  # arm64 or amd64, as the release names them
package="teamcity_linux_${arch}.deb"
checksums="teamcity-${VERSION}-checksums.txt"
release="https://github.com/JetBrains/teamcity-cli/releases/download/v${VERSION}"

cd /tmp
curl -fsSL -o "$package" "$release/$package"
[ -f "$OLDPWD/$checksums" ] && cp "$OLDPWD/$checksums" . || curl -fsSL -o "$checksums" "$release/checksums.txt"
grep " ${package}\$" "$checksums" | sha256sum -c -
dpkg -i "$package"
rm -f "$package" "$checksums"
cd "$OLDPWD"

apt-get clean
rm -rf /var/lib/apt/lists/*

# The CLI's configuration, which is all it needs to work here. The server is
# addressed over https, as usual: the egress proxy terminates that TLS with a
# certificate from the egress CA, which the bottle trusts, and attaches the
# access token (see bottle/egress.py). One thing in it is bottle's doing:
#
#   The token is a placeholder, and no secret: the CLI refuses to make a
#   request until it has been logged in to a server, and the proxy replaces
#   this header on the way out. bottle delivers nothing here -- an injected
#   credential never enters a bottle -- so what a tool needs in order to
#   believe it is configured is the feature's business, which is this.
#
# SERVER is the feature's option, a URL or a host; strip the scheme, keep any
# port, and drop anything after the host.
host=${SERVER#*://}
host=${host%%/*}
case "$host" in
  *[!A-Za-z0-9.:-]* | "" )
    echo "teamcity: server must be a host or a URL, not '$SERVER'" >&2
    exit 1 ;;
esac

home="$_REMOTE_USER_HOME"
install -d -o "$_REMOTE_USER" -g "$_REMOTE_USER" "$home/.config" "$home/.config/tc"
{
  echo "default_server: https://$host"
  echo "analytics: false"  # a sandbox shouldn't phone home about what the agent ran
  echo "servers:"
  echo "  \"https://$host\":"
  echo "    token: the-egress-proxy-holds-the-real-one"
  # READONLY is the feature's option, and on by default: the CLI refuses
  # anything but GET, so an agent can look at builds without starting or
  # changing one. `readOnly=false` hands it the rest; a server-side token with
  # read-only permissions is the boundary that doesn't depend on the CLI.
  [ "$READONLY" = true ] && echo "    ro: true" || true
} > "$home/.config/tc/config.yml"
chown "$_REMOTE_USER:$_REMOTE_USER" "$home/.config/tc/config.yml"
