# Interactive SSH logins start in the project instead of the home directory.
case $- in
  *i*) [ -n "${SSH_CONNECTION:-}" ] && [ "$PWD" = "$HOME" ] && [ -d /workspace ] && cd /workspace ;;
esac
