#!/bin/sh
set -eu

# The repository is https; apt needs root certificates to reach it.
apt-get update
apt-get install -y --no-install-recommends ca-certificates

# The signing key is vendored (fingerprint 31DD DE24 DDFA B679 F42D 7BD2 BAA9
# 29FF 1A7E CACE, per the Claude Code setup docs) so builds don't trust a key
# fetched at build time.
install -D -m 0644 claude-code.asc /etc/apt/keyrings/claude-code.asc
echo "deb [signed-by=/etc/apt/keyrings/claude-code.asc] https://downloads.claude.ai/claude-code/apt/stable stable main" \
  > /etc/apt/sources.list.d/claude-code.list

apt-get update
apt-get install -y --no-install-recommends claude-code
rm -rf /var/lib/apt/lists/*

# How this feature is authenticated, since JSON has no room for a comment:
#
#   Claude Code talks to https://api.anthropic.com as it always does. The
#   egress proxy terminates that TLS with a certificate from the egress CA,
#   which the bottle trusts (NODE_EXTRA_CA_CERTS, set by the base image), and
#   attaches the subscription token (see bottle/egress.py). So every call gets
#   it -- inference, and the organization's managed settings and policy
#   limits, which ignore ANTHROPIC_BASE_URL.
#
#   CLAUDE_CODE_OAUTH_TOKEN is the credential's inject.standin, so bottle puts
#   a fake token there when it creates a bottle -- and nowhere in the image, so
#   the throwaway bottle `bottle auth login claude` runs `claude setup-token`
#   in finds nothing to mistake for a login. The stand-in is what puts Claude
#   Code in subscription-token mode, so it sends the Authorization header (and
#   the OAuth beta header the API wants with such a token) that the proxy then
#   replaces on the way out.

# Settings for the bottle's user, so Claude starts working straight away:
# the permission mode (bypassPermissions by default: the bottle is the
# sandbox), no confirmation before bypass mode, a theme, no spinner tips, and
# the renderer.
#
# tui defaults to "default" rather than Claude Code's own "fullscreen".
# Fullscreen captures the mouse and copies a selection to the system clipboard
# instead of leaving it to the terminal -- via pbcopy, wl-copy, xclip or xsel,
# and OSC 52 when there are none. A bottle has none, and the OSC 52 fallback
# reaches a terminal that may not accept it (Terminal.app doesn't), so the
# selection is highlighted, copied nowhere, and silently lost.
#
# PERMISSIONMODE, THEME and TUI are the feature's options; their enums keep
# PERMISSIONMODE and TUI safe to write into JSON, THEME is checked here.
case "$THEME" in *[!a-z-]*) echo "claude: theme must be a theme name like dark, not '$THEME'" >&2; exit 1 ;; esac
home="$_REMOTE_USER_HOME"
install -d -o "$_REMOTE_USER" -g "$_REMOTE_USER" "$home/.claude"
cat > "$home/.claude/settings.json" <<JSON
{
  "permissions": { "defaultMode": "$PERMISSIONMODE" },
  "skipDangerousModePermissionPrompt": true,
  "theme": "$THEME",
  "tui": "$TUI",
  "spinnerTipsEnabled": false
}
JSON

# Claude's own state file: first-run setup done, and /workspace trusted, so its
# project settings apply without the trust dialog.
cat > "$home/.claude.json" <<JSON
{
  "hasCompletedOnboarding": true,
  "projects": { "/workspace": { "hasTrustDialogAccepted": true } }
}
JSON

# Claude's user instructions are bottle's context for the agent, which bottle
# writes to ~/BOTTLE.md when it creates the bottle.
ln -sfn "$home/BOTTLE.md" "$home/.claude/CLAUDE.md"
chown -h "$_REMOTE_USER:$_REMOTE_USER" "$home/.claude/settings.json" "$home/.claude.json" "$home/.claude/CLAUDE.md"
