#!/bin/sh
set -eu

# --break-system-packages so this works with the python feature's
# externallyManaged on too; it's the image, not a project.
case "$PLAYWRIGHT" in ""|*[!0-9.]*) echo "chromium: playwright must be a version like 1.64.1, not '$PLAYWRIGHT'" >&2; exit 1 ;; esac
python3 -m pip install --no-cache-dir --break-system-packages --root-user-action=ignore "playwright==$PLAYWRIGHT"

# Playwright's own Chromium, the build this release is made for, with the
# system libraries it needs (--with-deps). Only the headless shell: a bottle
# has no display. In the image, where every bottle has it, rather than
# ~/.cache, which a pause loses; containerEnv points Playwright at it.
PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright python3 -m playwright install --with-deps --only-shell chromium
# The agent's, as its own ~/.cache would be: `playwright install` takes a lock
# in it even when there's nothing to do, and without one hangs, retrying.
chown -R "$_REMOTE_USER:$_REMOTE_USER" /opt/ms-playwright
rm -rf /var/lib/apt/lists/*
