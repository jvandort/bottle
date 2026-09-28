# TODO

Things discussed but not built yet, roughly grouped. Not in priority order.

## Bottle lifecycle

- **Stop idle bottles.** A bottle runs until `bottle stop`, `bottle shutdown`
  or the host stops it. bottled could stop bottles that have been idle for a
  while, once "idle" is defined (no exec sessions, no egress traffic).
- **Resource limits.** Bottles get the whole Mac (every core, all memory), so
  a busy bottle can slow the host, and memory a guest has touched isn't
  returned to the host until the bottle stops, so a bottle holds its
  high-water mark for as long as it runs. Make CPUs and memory configurable,
  per bottle and as a repo default; consider a lower default than "everything"
  once several bottles at once is normal. Worth measuring what `container`
  actually returns first (no free-page reporting or ballooning is assumed).
- **Reconcile egress periodically.** bottled restores egress for running
  bottles when it starts, but a bottle started outside bottle (e.g.
  `container start`) has no network until a bottle command touches it.
- **bottled after an upgrade** keeps running the old code until
  `bottle shutdown`. Detect a version mismatch and restart it. Worse than it
  sounds: an old bottled serving the current hooks/pre-receive passes no
  `BOTTLE_NAME`, so the hook fails closed and every push from every bottle is
  refused, with an error that blames the push.
- **No `bottle daemon restart`.** Picking up new bottled code means
  `bottle shutdown`, which stops every running bottle first. Restarting bottled
  alone would do: it restores egress for running bottles when it starts.
- **Supervision.** Optionally run bottled as a launchd user agent (no sudo) for
  restart-on-crash and start-at-login.

## Repos and git

- **Unsaved-work check counts unreachable commits as saved.** `bottle delete`
  and `reset` ask whether the repo has each branch tip, with `git cat-file -e`,
  which succeeds for an object no ref names. So a branch pushed to
  `bottle-NAME/*` and then deleted on the host still reads as saved, and the
  commits go when the host next gcs. Check reachability instead.
- **Objects a bottle uses aren't protected from gc.** A bottle reads the
  repo's objects in place. If the host deletes a branch a bottle checked out
  and `git gc` later prunes its commits (after git's grace periods, weeks to
  months), that checkout breaks; `bottle reset` starts over. Accepted.
- **A bottle's refs outlive it.** `bottle delete` leaves
  `refs/namespaces/bottle-NAME/*` behind (and their mirrors), since they may be
  the only copy of the work. A later bottle with the same name owns the same
  namespace, and may force-update those refs without anything warning it.
  Consider treating names with leftover refs as taken, or keying the namespace
  by bottle id.
- **Sending work in.** There's no way to send new host commits into an existing
  bottle short of fetching `host` from inside it.
- **Keeping the repo tidy.** A bottle's refs are never deleted automatically:
  they stay after `bottle delete`, and after the bottle deletes the branch
  behind one (the mirror is never removed either, since the bottle can't
  delete). Commands:
  - `bottle refs [REPO]`: every bottle-owned ref, grouped by bottle, marked
    live, deleted, or gone from the bottle.
  - `bottle prune [BOTTLE]`: delete the refs of deleted bottles and gone
    branches, by default only those already reachable from the host's own
    branches; `--force` for the rest, after listing them.
- **Reviewing a bottle's work.** Where work surfaces on the host, where you
  edit it, and how you track what you've already read:
  [docs/design/reviewing-a-bottles-work.md](design/reviewing-a-bottles-work.md).
  Getting it out of a bottle at all, before anyone reviews it, is
  [docs/design/durability.md](design/durability.md). Until that's
  settled, hooks/post-receive mirrors each pushed branch to
  `refs/heads/bottle-NAME/*` so there's something to look at; nothing should
  depend on those mirrored refs, and the mirror goes when the real answer
  lands.
- **`bottle adopt`.** Turn a bottle's `bottle-NAME/X` into your own branch `X`:
  re-sign its commits on the host with your key (the agent never signs as
  you), optionally add `Signed-off-by`, and never overwrite an existing branch.
- **Pulling from a bottle.** Work only reaches the host when something inside
  pushes. A bottle that died mid-task, or an agent that never pushes, leaves
  commits that `bottle exec NAME git push host` can still rescue -- but only
  while the bottle starts. A host-initiated pull would not need that.
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
- **Proxy configuration for tools that ignore `*_PROXY`:** JVM system
  properties (Gradle, Maven), `NODE_USE_ENV_PROXY=1` for Node's fetch, and a
  `ProxyCommand` (plus `netcat-openbsd`) for git over SSH. Tools that do read
  the variables see them under sudo now (the base image's sudoers env_keep).
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

The channel exists: requests to `http://bottle.host/` are answered by the
bottle's egress proxy on the host (bottle/host.py), and the first service is
the repo as a read-only git origin. More services can be added there, e.g.: bottle decides which services exist; the bottle can only request them,
and the host can require approval. Candidates:

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
- The repo at `/workspace`: which repo, branch and commit it started from, and
  that `git push host` hands work back. (The push rules are in ~/BOTTLE.md
  already; the rest of this list isn't.)
- What's installed (the image's tools), and what isn't available.
- Which host commands exist, once they do.

## Agents

- Starting an agent in a bottle (in tmux, so it survives disconnects) and
  reattaching.
- **Open login URLs on the host.** A `$BROWSER` script in the base image that
  asks bottled (e.g. via `http://bottle.host/open?url=...` through the egress
  proxy) to `open` the URL on the Mac, so `bottle auth login` needs no copying.
- **Keep credentials out of bottles.** Today a delivered credential is readable
  by the agent. Deliver a stand-in instead and have bottled add the real one
  (see credentials via the proxy).
- **Leftover throwaway bottles.** `bottle auth login` removes its throwaway
  bottle on exit, even when interrupted, but not if bottle itself is killed;
  clean up `bottle-throwaway-*` containers and networks.
- **Credential files.** Only environment variables are delivered; add file
  delivery (and file capture at login) for tools that read credential files.

## Remote IDEs and SSH

- Inject a login key (`authorized_keys`) at `bottle new`.
- Pin each bottle's host key on the host after first boot.
- Generate `~/.ssh/config` entries (`Host bottle-<name>`), with a
  `ProxyCommand` so IDEs (VS Code, Zed, JetBrains Gateway) connect without
  managing IPs.
- Persistent cache volumes for IDE servers (`~/.vscode-server` etc.).

## Images

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

## Portability

- **Another VM runtime.** bottle is macOS and apple/container only, which is
  all this team needs. If others want it, the same model (a microVM per
  bottle, a read-only object mount, an egress proxy on the host) fits Linux
  microVM runtimes over KVM. Keep the `container` calls behind
  `bottle/runtime.py` so a second backend can be grafted on rather than
  threaded through: today that means no `container`-shaped assumptions
  leaking into `bottles.py`, `images.py` or `daemon.py`, and host-specific
  bits (`sysctl hw.memsize`, Homebrew, the Keychain, launchd) named as such.

## Diagnostics

- `bottle doctor`: check prerequisites, and detect known problems such as
  another process holding port 53, which breaks `container`'s built-in DNS
  ([apple/container#402](https://github.com/apple/container/issues/402)).

## Known `container` quirks

- `container build --quiet` hangs indefinitely; never pass it.
- The builder VM keeps the DNS settings from whichever `build` created it.
- Resizing the terminal during a non-TTY `container run` prints a harmless
  "failed to send signal" error.
