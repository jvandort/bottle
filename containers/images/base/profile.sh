# Interactive SSH logins start where the bottle says work happens (its first
# mount: BOTTLE_WORKDIR, set by bottle) instead of the home directory.
case $- in
  *i*) [ -n "${SSH_CONNECTION:-}" ] && [ "$PWD" = "$HOME" ] && [ -d "${BOTTLE_WORKDIR:-}" ] && cd "$BOTTLE_WORKDIR" ;;
esac
