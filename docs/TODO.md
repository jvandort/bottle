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
  the wrapped repo. The bottle can also reach any other object in the repo by
  hash, e.g. by checking out another commit it learned about. Those objects
  aren't pinned: if the host deletes or rewrites the branch they're on and the
  repo is gc'd, they disappear from under the bottle and its checkout breaks.
  Work the agent commits itself is safe; its objects live in the bottle.
- **Getting work back out.** Fetch the bottle's commits into the host repo
  without SSH, e.g. a git transport over `container exec`
  (`git-upload-pack` inside the bottle), or bundles.
- **Commit identity and signing.** Bottles have no git identity; decide who
  commits (agent identity, `Signed-off-by`), and sign on the host after
  fetching, so signing keys never enter the bottle.
- **Unwrap / moved repos.** No way to unwrap a repo or update its path.

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

## Agents

- An image with agent CLIs installed.
- Starting an agent in a bottle (in tmux, so it survives disconnects) and
  reattaching.
- API key / login handling (see credentials via the proxy).

## Remote IDEs and SSH

- Inject a login key (`authorized_keys`) at `bottle new`.
- Pin each bottle's host key on the host after first boot.
- Generate `~/.ssh/config` entries (`Host bottle-<name>`), with a
  `ProxyCommand` so IDEs (VS Code, Zed, JetBrains Gateway) connect without
  managing IPs.
- Persistent cache volumes for IDE servers (`~/.vscode-server` etc.).

## Images

- **Rebuild stale images.** `bottle new` builds missing images but doesn't
  notice outdated ones (a changed `containers/` dir, or a rebuilt parent).
  Label images with a hash of their inputs and compare.
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
