#!/bin/sh
# bottle/base: the bottle contract, and nothing else. Features add the rest.
#
# The contract is what bottle's own commands rely on inside a bottle:
#   - the genie user, UID 1000, with passwordless sudo
#   - /workspace, owned by genie: the checkout
#   - git: setting up /workspace, pushing work back to the host, and
#     `bottle delete`'s check for unpushed work
#   - sshd, for IDEs
#   - ca-certificates, so the entrypoint can trust the machine's CA, which
#     bottle-cli gives at /run/bottle/ca.crt, when the bottle starts;
#     NODE_EXTRA_CA_CERTS points at the store's bundle, so Node tools (Claude
#     Code among them) trust it too
# bottle verifies it whenever a bottle starts (CONTRACT in folio/folio/bottle/bottles.py);
# keep the two in sync.
#
# A feature like the others, installed first and on its own, on Debian 13 by
# name (DEBIAN in folio/folio/bottle/images.py), so every bottle shares it. What a bottle
# runs when it starts, bottle-entrypoint and sshd, is bottle's own (ENTRYPOINT
# and CMD there).
#
# Its containerEnv: LANG, and NODE_EXTRA_CA_CERTS. Node trusts its own roots,
# and NODE_EXTRA_CA_CERTS: the system bundle, which update-ca-certificates
# rebuilds with the machine's CA when a bottle starts (entrypoint.sh).
#
# Its packages are its customizations.bottle.apt's, installed before this runs.
set -eu

groupadd --gid 1000 genie
useradd --uid 1000 --gid genie --create-home --shell /bin/bash genie
# Passwordless sudo, and the sudoers files checked.
echo 'genie ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/genie
chmod 0440 /etc/sudoers.d/genie
visudo -c -q
install -d -o genie -g genie /workspace
# The agent commits as itself, never as you. bottle.host isn't a real domain.
su genie -c 'git config --global user.name genie && git config --global user.email genie@bottle.host'

install -m 0644 sshd_config /etc/ssh/sshd_config.d/bottle.conf
# Installing openssh-server generates host keys. Delete the defaults so bottles
# don't share one: entrypoint.sh makes a fresh key on first boot.
rm -f /etc/ssh/ssh_host_*
# Drop AcceptEnv to stop clients forwarding locales the image doesn't have.
sed -i '/^AcceptEnv/d' /etc/ssh/sshd_config

install -m 0755 entrypoint.sh /usr/local/bin/bottle-entrypoint
install -m 0644 profile.sh /etc/profile.d/bottle.sh
