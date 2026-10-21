# TODO

Things discussed but not built yet, roughly grouped. Not in priority order.

## Bottle lifecycle

- **Stop idle bottles.** A bottle runs until `bottle stop`, `bottle shutdown`
  or the host stops it. bottled could stop bottles that have been idle for a
  while, once "idle" is defined (no exec sessions, no egress traffic).
- **Resource limits.** Memory can be limited per repo (`--memory` on `repo
  add`/`set`/`update`) and per bottle (`new --memory`), but bottles still get
  every core, so a busy bottle can slow the host. Make CPUs configurable the
  same way. Without a limit, a bottle gets all of the machine's memory, and
  memory a guest has touched isn't returned to the host until the bottle
  stops; consider a lower default than "everything" once several bottles at
  once is normal. Worth measuring what `container` actually returns first (no
  free-page reporting or ballooning is assumed).
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

- **Objects a bottle uses aren't protected from gc.** A bottle reads the
  repo's objects in place. If the host deletes a branch a bottle checked out
  and `git gc` later prunes its commits (after git's grace periods, weeks to
  months), that checkout breaks, and a new bottle starts over. Accepted.
- **The `heads` view serves every ref, not just branches.** It's the repo as
  it is, so a bottle served it sees every namespace in the repo -- other
  bottles' lanes included -- and your `refs/remotes/*`. The README and host.py
  describe it as your own branches, so what's served is wider than what's
  documented. Narrow it to `refs/heads/*` (and tags), or document it.
- **Commit identity and signing.** Bottles have no git identity; decide who
  commits (agent identity, `Signed-off-by`), and sign on the host after
  fetching, so signing keys never enter the bottle.

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
- **Reuse networks, so the anchor stays put.** A container's networks are
  fixed when it starts, so every bottle network made or removed boots a new
  anchor VM (1.1-1.5s) and deletes the old one (0.4-0.5s): about 1.5s of each
  `bottle new` (4.0s, image built) and of each `delete` (2.1s), timed from
  bottle.log. Keep a deleted bottle's network as a spare, the anchor still on
  it, and give the next bottle a spare if there is one: then a delete-then-new
  never touches the anchor. Folio does exactly that on every pause and resume.
- **Build-time egress exposure.** During `bottle build`, the temporary proxy
  on the default network is usable by any container on that network.
- **Credentials via the proxy: what's left.** The proxy terminates TLS for the
  hosts a credential names (the egress CA, bottle/ca.py), so tools use https as
  usual. What's left:
  - One request per terminated tunnel: the upstream is told
    `Connection: close`, so a client opens a new tunnel (and handshake) per
    request, and an `Upgrade` (WebSockets) through a terminated tunnel doesn't
    work. Reading responses' framing would allow keep-alive.
  - A bottle picks up a re-login only when it next starts.
  - An injected host is allowed even when private, which is wider than the
    `--allow` list it bypasses.

## Host commands for the genie

The channel exists: requests to `http://bottle.host/` are answered by the
bottle's egress proxy on the host (bottle/host.py), and the first service is
the repo as a read-only git origin. More services can be added there, e.g.: bottle decides which services exist; the bottle can only request them,
and the host can require approval. Candidates:

- **Ask the user:** request approval, or a decision, and wait for the answer.
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
- The repo at `/workspace`, its branch, and its remotes are in ~/BOTTLE.md
  already (how to work with them is the bottle's creator's to add); the rest
  of this list isn't.
- What's installed (the image's tools), and what isn't available.
- Which host commands exist, once they do.

## Agents

- **Open login URLs on the host.** A `$BROWSER` script in the base image that
  asks bottled (e.g. via `http://bottle.host/open?url=...` through the egress
  proxy) to `open` the URL on the machine, so `bottle auth login` needs no copying.
- **Leftover throwaway bottles.** `bottle auth login` removes its throwaway
  bottle on exit, even when interrupted, but not if bottle itself is killed;
  clean up `bottle-throwaway-*` containers and networks.
- **Credentials a proxy can't attach.** A credential is spent as a request
  header the egress proxy attaches, which suits token-in-a-header APIs and
  nothing else. A tool that signs its requests, or one reached over a protocol
  the proxy can't read (ssh, or TLS the bottle pins rather than trusting the
  egress CA), has no way to use one. Whether
  those are worth supporting -- and how, without handing the bottle the secret
  -- is open.

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
