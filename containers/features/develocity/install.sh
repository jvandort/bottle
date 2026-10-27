#!/bin/sh
set -eu

# Develocity serves MCP itself, over streamable HTTP at <server>/mcp, so there
# is nothing to install: this adds each server's to Claude Code's user config,
# as develocity-<host> (develocity-ge-example-com).
# It runs no `claude`, so it doesn't need the claude feature: without it, the
# config waits for a Claude Code installed some other way, and the access key
# is attached to the server's API all the same.
[ -n "$SERVER" ] || {
  echo "develocity: which server? use --feature develocity:server=https://develocity.example.com" >&2
  exit 1
}

# SERVER is the feature's option: hosts or URLs, separated by commas. Of each,
# strip the scheme, keep any port, and drop anything after the host.
hosts=""
set -f  # a server is no glob
IFS=,
for server in $SERVER; do
  server=$(echo "$server" | sed 's/^ *//; s/ *$//')
  [ -n "$server" ] || continue
  host=${server#*://}
  host=${host%%/*}
  case "$host" in
    *[!A-Za-z0-9.:-]* | "" )
      echo "develocity: server must be hosts or URLs, separated by commas, not '$SERVER'" >&2
      exit 1 ;;
  esac
  hosts="$hosts $host"
done
unset IFS
[ -n "$hosts" ] || {
  echo "develocity: server must be hosts or URLs, separated by commas, not '$SERVER'" >&2
  exit 1
}

apt-get update
apt-get install -y --no-install-recommends jq
apt-get clean
rm -rf /var/lib/apt/lists/*

# Each server, as `claude mcp add --transport http --scope user` would write it,
# merged into ~/.claude.json: the one the claude feature wrote, when it's here
# (this feature installs after it), else a new one. Not `claude mcp add`
# itself: run here, it also writes a machine and user ID, which every bottle
# from the image would share.
#
# How it's authenticated: the token is a placeholder, and no secret (see
# bottle/features.py, STANDIN). bottled terminates the TLS to the server with a
# certificate from the egress CA, which the bottle trusts, and puts the access
# key in place of this one on the way out (see bottle/egress.py), so the key
# never enters a bottle.
config="$_REMOTE_USER_HOME/.claude.json"
[ -f "$config" ] || echo '{}' > "$config"
for host in $hosts; do
  name="develocity-$(echo "$host" | tr '.:' '--')"
  jq --arg name "$name" --arg url "https://$host/mcp" '.mcpServers[$name] = {
    type: "http", url: $url,
    headers: {Authorization: "Bearer the-egress-proxy-holds-the-real-one"}
  }' "$config" > "$config.new"
  mv "$config.new" "$config"
done
chown "$_REMOTE_USER:$_REMOTE_USER" "$config"

# Gradle's Develocity plugin, the same way: a keys.properties line for each
# server, with the placeholder for its key, where the plugin looks for one on
# the machine too, so a build authenticates as it would there, with nothing to
# set. By bare host, as the plugin looks a server up (and a `:` in a key would
# end it). A server it already has a line for keeps it.
keys="$_REMOTE_USER_HOME/.gradle/develocity/keys.properties"
mkdir -p "$(dirname "$keys")"
touch "$keys"
for host in $hosts; do
  host=${host%%:*}
  cut -d= -f1 "$keys" | grep -qxF "$host" || echo "$host=the-egress-proxy-holds-the-real-one" >> "$keys"
done
chown -R "$_REMOTE_USER:$_REMOTE_USER" "$_REMOTE_USER_HOME/.gradle"
