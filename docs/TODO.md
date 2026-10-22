# TODO

Things discussed but not built yet, roughly grouped. Not in priority order.

## Bottle lifecycle

- **Stop idle bottles.** A bottle runs until `bottle stop`, `bottle shutdown`
  or the host stops it. bottled could stop bottles that have been idle for a
  while, once "idle" is defined (no exec sessions, no egress traffic).
- **Resource limits.** Memory can be limited per repo (`--memory` on `repo
  add`/`set`/`update`) and per bottle (`new --memory`), but bottles still get
  every core, so a busy bottle can slow the host. Make CPUs configurable the
  same way. Every spec gives its memory (folio's default is 2G), but memory a
  guest has touched comes back as the guest frees it. Measured on a Mac
  (smolvm 1.31.2, 2026-10-22, a 16G bottle running a Gradle build):
  - Reclaim works, and is safe: freed guest pages are released
    (MADV_FREE_REUSABLE), and under memory pressure macOS took 7+ GB back
    in ~20s. Data the guest still uses is kept or compressed, never
    dropped (a 4 GB file's checksum held through pressure).
  - Activity Monitor and `footprint` can't be trusted for a smolvm VM:
    libkrun never calls MADV_FREE_REUSE, so reused pages go uncharged
    (2.3 GB shown with 13.5 GB used), and other pages are charged but not
    resident (20 GB shown for a 16 GB guest). `vmmap --summary` resident +
    swapped for "shared memory", or `top -l 1 | grep PhysMem`, is closer.
    Reported: https://github.com/smol-machines/smolvm/issues/1944
    (the issue has a reproducer).
  - The guest is slow to give memory up. After a build, ~4.5 GB of free
    memory was in pieces under 2 MB, which free-page reporting skips
    (page_reporting_order 9), and ~5 GB was page cache, which only
    smolvm's idle pulse evicts, after 10 minutes under ~1% of a core.
    Compaction and dropping caches by hand took the footprint from 14 GB
    to 4 GB. Options: a lower page_reporting_order (/sys is read-only in
    the container; root can remount it), vm.compaction_proactiveness, a
    shorter SMOLVM_IDLE_RECLAIM, or bottle dropping caches when a session
    ends. Untested which helps without slowing builds.
- **`bottle exec`'s status mixes smolvm's failures with the command's.** A
  smolvm failure (no such machine, say) exits 1, as a command can. Exit 255
  for bottle's and smolvm's own failures, as ssh does, would let folio tell
  them apart.
- **`bottle auth login` on smolvm** (a login command in a throwaway machine)
  is tested with a fake, not yet run on a Mac.
- **Give memory back when the Mac asks for it.** macOS announces memory
  pressure (normal, warning, critical) to anything listening:
  DISPATCH_SOURCE_TYPE_MEMORYPRESSURE, or the level as a sysctl (check the
  name; `kern.memorystatus_vm_pressure_level`, it's thought). bottled could
  listen, and at warning have running bottles give back what only the guest
  can: drop its page cache and compact its free memory (as root in the
  guest: `echo 3 > /proc/sys/vm/drop_caches`, `echo 1 >
  /proc/sys/vm/compact_memory`), or pulse smolvm's balloon through the
  machine's control socket. Bottles idle longest first, so a running build
  keeps its cache. Better than smolvm's own trigger, 10 minutes under ~1%
  of a core, which a bottle with an agent attached rarely meets; and macOS
  already takes memory the guest has released, so this is the rest.
- **Machines started outside bottle aren't mediated.** A bottle's machine
  asks bottled about its connections only when bottle starts it, with the
  decider and a token. `smolvm machine start` by hand starts it with smolvm's
  own network policy instead, and none of bottle's.
- **bottled after an upgrade** keeps running the old code until
  `bottle shutdown`. Detect a version mismatch and restart it. Worse than it
  sounds: an old bottled serving the current hooks/pre-receive passes no
  `BOTTLE_NAME`, so the hook fails closed and every push from every bottle is
  refused, with an error that blames the push.
- **No `bottle daemon restart`.** `bottle daemon stop` and any bottle command
  picks up new bottled code, and running bottles keep their tokens, but their
  connections fail in between. A restart that starts the new one first would
  close that gap.
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
- **Guest TCP connections cost more than they should (smolvm).** smolvm
  carries the guest's TCP in a userspace stack (smoltcp, `tcp_relay.rs`), not
  TSI, and each connection holds a slot in a per-machine table:
  - The table is 512 by default; a Gradle build ran out, and a connection
    past it is refused at its SYN. Raised to 4096, the most smolvm takes
    (SMOLVM_TCP_RELAY_CAPACITY in smolvm.py).
  - A slot is held until smolvm's side is Closed, so a connection the far
    end closed first keeps it through smoltcp's 10s TIME_WAIT: the cap is
    really on connections opened in the last ~10s. TIME_WAIT on a virtual
    wire inside one process protects nothing; not counting it, or ending it
    at once (`cleanup_closed`), would be a small smolvm change.
  - Each connection is a host thread (blocking relay), which is why the cap
    exists. An event loop (kqueue) would make it a socket and buffers, and
    TSI (libkrun's guest-connect-to-host-socket) no stack at all; both are
    rewrites, and TSI would need the decider hook added.
  - smolvm asks the decider with what arrived in its first 100ms
    (FIRST_PAYLOAD_HOLD), so a busy guest's ClientHello can come after:
    bottled now reads it itself for a credential's port (OPENING in
    egress.py). A longer hold, or one the decider can extend, would be
    smolvm's fix.
- **Credentials: what's left.** bottled terminates TLS for the
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
bottled on the host (bottle/host.py), and the first service is
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
- Which destinations are allowed (~/BOTTLE.md says only that private ones aren't).
- The repo at `/workspace`, its branch, and its remotes are in ~/BOTTLE.md
  already (how to work with them is the bottle's creator's to add); the rest
  of this list isn't.
- What's installed (the image's tools), and what isn't available.
- Which host commands exist, once they do.

## Agents

- **Open login URLs on the host.** A `$BROWSER` script in the base image that
  asks bottled (e.g. via `http://bottle.host/open?url=...`) to `open` the URL on the machine, so `bottle auth login` needs no copying.
- **Leftover throwaway bottles.** `bottle auth login` removes its throwaway
  bottle on exit, even when interrupted, but not if bottle itself is killed;
  clean up `bottle-throwaway-*` machines and their decider tokens.
- **Credentials bottled can't attach.** A credential is spent as a request
  header bottled attaches, which suits token-in-a-header APIs and
  nothing else. A tool that signs its requests, or one reached over a protocol
  bottled can't read (ssh, or TLS the bottle pins rather than trusting the
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
- **Share image layers and repo objects between machines, with DAX (smolvm
  feature request).**
  Each machine unpacks its image onto its own disk
  (/storage/image-archives/packed_layers, on the machine's /dev/vda), so
  five bottles are five unpacked copies on disk, in the Mac's file cache,
  and in each guest's page cache. smolvm already shares the image archive
  (one copy in its cache, mounted over virtiofs: packed_layers_dir_for_ref)
  and, for `smolvm pack` artifacts only, unpacked layers from one directory
  on the Mac (vms/_shared). Asking for the same for machines made from an
  image would make the Mac's copy one. With virtiofs DAX, guests would map
  the Mac's cached pages instead of copying them into their own page cache:
  one copy of the image for every bottle, at memory speed, as clean file
  cache macOS can drop; the repo's objects, already on virtiofs, would need
  only DAX. smolvm leaves DAX out on arm64 (VIRTIOFS_DAX_SUPPORTED, "not
  correct on architectures with virtually mapped caches"), and its arm64
  guest kernel has no ZONE_DEVICE, FS_DAX or FUSE_DAX; Linux 6.18's Kconfig
  excludes no architecture, and libkrun has a macOS DAX path, so ask them
  why. Measured in a Gradle bottle
  (fincore, 2026-10-22): 2.6 GB of guest page cache, of which the image
  ~1.1 GB (a JDK 619 MB, claude 279 MB) and the repo's objects 992 MB.
- **Disk usage.** Every image is an OCI archive in `~/.bottle/images`, and
  each machine unpacks its own copy, so layers aren't shared once unpacked.
  Worth watching as layers multiply.
- **Faster checkouts** for large repos, e.g. an APFS-cloned, pre-checked-out
  volume per repo.

## Portability

- **Another host.** bottle is macOS on Apple silicon, which is all this team
  needs. smolvm also runs on Linux over KVM; what's Mac-specific is named as
  such (`sysctl hw.memsize`, Homebrew, the Keychain).

## Diagnostics

- `bottle doctor`: check prerequisites, and known problems: smolvm's version,
  the decider's port taken, a resolver smolvm can't reach (it takes the Mac's
  first IPv4 nameserver when a machine starts).

## Known smolvm quirks

Checked by `tests/test_smolvm_contract.py` for a new smolvm version. The ones
that bite:

- `exec` and `machine run` without `-i` hold all output back and cut it at
  11 MiB, silently; bottle always passes `-i`.
- A machine's DNS resolver is fixed when it starts: the first IPv4
  nameserver in the Mac's `/etc/resolv.conf`, so a VPN that moves the
  system resolver (Cloudflare WARP) must stay connected while bottles run,
  and macOS's scoped resolvers (`/etc/resolver/*`) don't apply. smolvm
  answers every guest query itself, UDP and TCP, and takes every guest TCP
  connection to port 53 whatever its address, so DNS can't go through the
  decider either. Ways to follow the Mac's resolver would need smolvm:
  `--dns ADDR:PORT` (bottled answering on 127.0.0.1:5353 with the Mac's
  lookup), or re-reading `/etc/resolv.conf` when it changes.
- smolvm's own failures exit 1, like a failing command. `--error-format
  json` puts them on stderr as `{"error":{"code":...}}`, which tells them
  apart; bottle passes it on every call.
- `--mem` of all the Mac's memory fails to start.
