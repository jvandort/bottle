# TODO

Things discussed but not built yet, roughly grouped. Not in priority order.

## Bottle lifecycle

- **Stop idle bottles.** A bottle runs until `bottle stop`, `bottle shutdown`
  or the host stops it. bottled could stop bottles that have been idle for a
  while, once "idle" is defined (no exec sessions, no egress traffic).
- **Resource limits.** Bottles get `container`'s defaults (4 CPUs, 1 GB).
  Make CPUs and memory configurable per bottle or per image.
- **Reconcile egress periodically.** bottled restores egress for running
  bottles when it starts, but a bottle started outside bottle (e.g.
  `container start`) has no network until a bottle command touches it.
- **bottled after an upgrade** keeps running the old code until
  `bottle shutdown`. Detect a version mismatch and restart it.
- **Supervision.** Optionally run bottled as a launchd user agent (no sudo) for
  restart-on-crash and start-at-login.

## Repos and git

- **Pinning only covers the starting commit.** `refs/bottle/<id>` keeps the
  bottle's starting commit (and its history) from being garbage-collected in
  the repo. The bottle can also reach any other object in the repo by
  hash, e.g. by checking out another commit it learned about. Those objects
  aren't pinned: if the host deletes or rewrites the branch they're on and the
  repo is gc'd, they disappear from under the bottle and its checkout breaks.
  Work the agent commits itself is safe; its objects live in the bottle.
- **Fetched refs outlive their bottle.** `bottle git fetch` writes
  `refs/remotes/bottle-NAME/*`, and `bottle delete` leaves them, since they may
  be the only copy of the work. A later bottle with the same name fetches into the
  same refs; fetches are additive, so a clash is refused rather than overwriting,
  but it's confusing. Consider treating names with leftover fetched refs as
  taken, or namespacing by bottle id.
- **Fetch tags and pushing in.** `bottle git fetch` skips tags, and there's no way yet
  to send new host commits into an existing bottle.
- **Keeping the repo tidy.** Fetched refs (`bottle-NAME/*`) are never
  deleted automatically: they stay after `bottle delete`, and after the bottle
  deletes a branch (`fetch` reports these as gone). Pins (`refs/bottle/<id>`)
  are removed only by `bottle delete`; never by any cleanup command, since a
  pin that looks orphaned may belong to another `BOTTLE_HOME`. Commands:
  - `bottle git refs [REPO]`: every bottle-owned ref, grouped by bottle, marked
    live, deleted, or gone from the bottle.
  - `bottle git prune [BOTTLE]`: delete fetched refs of deleted bottles and gone
    branches, by default only those already reachable from the host's own
    branches; `--force` for the rest, after listing them.
  - `bottle git status NAME`: per bottle branch, ahead of what was fetched, and
    whether it's merged into a host branch.
  - `bottle git log NAME` / `bottle git diff NAME`: the bottle's work since its
    starting commit, for review.
- **`bottle git adopt NAME BRANCH [LOCAL]`.** Turn a fetched branch into a real
  local branch (never overwriting one), optionally adding `Signed-off-by` and
  re-signing commits on the host. Fetched refs stay visible to
  `git branch -r` for now; adopt should become the usual way in.
- **A git remote helper.** A `git-remote-bottle` executable would let plain git
  (and IDEs) fetch with URLs like `bottle::gradle`, starting the bottle and
  applying the additive rules, without enabling the `ext::` transport in the
  repo's config.
- **Commit identity and signing.** Bottles have no git identity; decide who
  commits (agent identity, `Signed-off-by`), and sign on the host after
  fetching, so signing keys never enter the bottle.
- **Removing and moving repos.** No `bottle repo remove`, and no way to update
  a repo's path except editing `repos.json`.
- **Global default features** (e.g. an agent for every repo), applied before a
  repo's own.

## Network

- **Configurable egress policy.** Allowlisted private hostnames
  (`--allow`-style) from a config file, e.g. `~/.bottle/config.toml`, rather
  than flags; possibly per-bottle.
- **Proxy configuration for tools that ignore `*_PROXY`:** apt config, JVM
  system properties (Gradle, Maven), `NODE_USE_ENV_PROXY=1` for Node's fetch,
  and a `ProxyCommand` (plus `netcat-openbsd`) for git over SSH.
- **Fail fast without the proxy.** Bottles have no DNS server, so a tool that
  ignores the proxy waits for a DNS timeout. An empty or local resolver
  config would make it fail immediately.
- **Optional in-bottle DNS**, forwarded to the host's resolver, for tools that
  resolve names themselves.
- **Block bottles from other host services.** A bottle's network reaches only
  the host, but any host service listening on all interfaces is reachable,
  not just the egress proxy. A `pf` rule (one-time sudo) could allow only the
  proxy port.
- **Build-time egress exposure.** During `bottle build`, the temporary proxy
  on the default network is usable by any container on that network.
- **Credentials via the proxy.** Inject API keys and tokens as request
  headers at the egress proxy, so they never enter a bottle.

## Host commands for the genie

A channel for the bottle to ask the host to do specific things, e.g. a socket
bottle injects into the bottle (vsock, or a published Unix socket), served by
bottled. bottle decides which commands exist; the bottle can only request them,
and the host can require approval. Candidates:

- **Hand back work:** push a branch to the host repo (as `bottle-NAME/*`, the
  same additive rules as `bottle git fetch`), so the agent can say "done" itself.
- **Ask the human:** request approval, or a decision, and wait for the answer.
- **Notify:** "finished", "blocked", "needs review", surfaced on the host.
- **Open something on the host:** a URL in the host browser, e.g. an OAuth or
  review page.
- **Request a credential:** a short-lived token, scoped and logged, rather
  than a long-lived secret in the bottle's environment.
- **Host-only tools:** run an allowlisted command on the host, e.g. one that
  needs host credentials or hardware.
- **Bottle status:** time or resource budget left, egress policy, which hosts
  are allowed.

## Context for the genie

Generate a description of its environment for the agent, e.g. an agent
instructions file (like `CLAUDE.md`) in the bottle's home or workspace:

- It's in a sandboxed Linux VM (bottle), as `genie`, with passwordless sudo.
- Network access is only via the HTTP proxy in `*_PROXY`; there's no DNS; which
  destinations are allowed.
- The repo at `/workspace`: which repo, branch and commit it started from; that
  work reaches the host via `bottle git fetch` (or a host command), and branches are
  fetched additively, so rewriting history gets refused.
- What's installed (the image's tools), and what isn't available.
- Which host commands exist, once they do.

## Agents

- Starting an agent in a bottle (in tmux, so it survives disconnects) and
  reattaching.
- Authentication for agent CLIs (see credentials via the proxy).
- Pre-seed agent config in images (e.g. Claude Code's first-run onboarding).

## Remote IDEs and SSH

- Inject a login key (`authorized_keys`) at `bottle new`.
- Pin each bottle's host key on the host after first boot.
- Generate `~/.ssh/config` entries (`Host bottle-<name>`), with a
  `ProxyCommand` so IDEs (VS Code, Zed, JetBrains Gateway) connect without
  managing IPs.
- Persistent cache volumes for IDE servers (`~/.vscode-server` etc.).

## Images

- **Rebuild stale images.** `bottle new` builds missing images but doesn't
  notice outdated ones (a changed `containers/images/` or
  `containers/features/` dir, or a rebuilt base). Label images with a hash of
  their inputs and compare.
- **Composing without rebuilding.** Each new combination of features re-runs
  every feature's install. If that gets slow, stack each feature's own layers
  onto the base instead (no build), with conflict detection for files several
  features change.
- **Runtimes and more layers:** language runtimes as separate stages copied in
  (Node, Python via uv, a JDK, Go), mise for per-project versions, and
  read-only dependency caches (e.g. `GRADLE_RO_DEP_CACHE`).
- **Disk usage.** `container` unpacks every image into its own disk image, so
  layers aren't shared once unpacked. Worth watching as layers multiply.
- **Faster checkouts** for large repos, e.g. an APFS-cloned, pre-checked-out
  volume per repo.

## Diagnostics

- `bottle doctor`: check prerequisites, and detect known problems such as
  another process holding port 53, which breaks `container`'s built-in DNS
  ([apple/container#402](https://github.com/apple/container/issues/402)).

## Known `container` quirks

- `container build --quiet` hangs indefinitely; never pass it.
- The builder VM keeps the DNS settings from whichever `build` created it.
- Resizing the terminal during a non-TTY `container run` prints a harmless
  "failed to send signal" error.
