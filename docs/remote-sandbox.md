# Remote sandboxes

Where bottle (and folio, which drives it) would split to run sandboxes on
another machine: give it a Linux host reachable over ssh, and a thread's
sandbox runs there while it's live and is kept on the Mac while it's paused.
Nothing here is built. This is the seam to keep when changing the code, so
that building it later moves one boundary instead of untangling many.

## The seam: control plane and runner

Some things belong to the user and stay on their Mac; some have to sit next
to the VM. The seam runs between them:

| Control plane (the user's Mac) | Runner (next to the VM: the Mac today, a Linux host later) |
| --- | --- |
| folio's threads, lanes and paused snapshots | the machines (smolvm, which runs on Linux over KVM too) |
| the user's repo, and the git service that reads and writes it | image builds and the image store |
| credentials (the Keychain), the egress CAs' keys, logins | the decider: smolvm asks it on host loopback, so it has to be on the same host |
| specs, features, the registry of bottles | deciding direct and deny, judged against the runner's own network |

Everything bottle does today falls on one side:

- **Driver** (`smolvm.py`, `prereqs.py`): machine lifecycle and exec. *Runner.*
- **Image builder** (`images.py`): builds the base and feature images with
  buildah in a throwaway machine, using only the driver's machines and exec
  (inputs in as a tar on stdin, the archive out on stdout), and keeps them as
  OCI archives. *Runner*, unchanged: exec is what crosses ssh.
- **Catalog** (`features.py`, `specs.py`, `containers/`): what features
  exist, how specs compose them. *Control plane*; the runner is handed what
  to build.
- **Gateway** (`egress.py`, `decider.py`, `daemon.py`): split down the
  middle; see below.
- **Credential broker** (`auth.py`, `ca.py`, `Injection`): *control plane*,
  so a credential never leaves the Mac.
- **Git exchange** (`host.py`, `hooks/pre-receive`, the checkout in
  `bottles.py`): *control plane*, since it writes the user's repo.
- **Orchestrator** (`bottles.py`, and folio): *control plane*. It asks the
  runner for machines and execs, and sets the guest up through them.

## The gateway splits between decide and serve

bottled answers each guest connection in two parts, already two methods:

- **`Gateway.decide(flow)`**: from the destination and the flow's first bytes
  (a TLS ClientHello's name, an HTTP Host), allow direct, deny, or redirect.
  It needs only policy. This runs on the runner.
- **`Gateway.serve(flow, decision, ...)`**: be the far end of a redirected
  flow. Either it serves `bottle.host`'s git remotes, or it terminates TLS for
  a credential's host, attaches the credential and connects to the real server.
  This needs the repo and the secrets, so it stays on the Mac.

A remote decider answers direct and deny itself, and carries each redirected
flow over the ssh connection to the Mac, where `serve` handles it as today.
Credentialed requests then leave from the Mac. That costs a round trip, and
it keeps the broker's promise.

The decider protocol between smolvm and bottled is in `decider.py`'s
docstring: a token per bottle, the machine's identity, the destination and up
to 8192 first bytes, and one byte back. smolvm sends at most what arrived in
its first 100 ms, so `decide` can be shown an incomplete ClientHello; for a
credential's port it then redirects (`OPENING`) and `serve` reads the rest.
That case goes to the Mac too.

## What crosses the connection

Everything between the two sides goes over one ssh connection:

- **Runner operations**: build an image (from a context the control plane
  sends), create, start, stop and delete a machine, report its state, and
  bind or release a bottle at the decider (token, policy, which flows to
  redirect). The runner keeps its own state: images, machines, tokens.
- **Exec**, byte-transparent both ways, with a TTY when attached: `ssh -t
  host smolvm machine exec -i ...`. Folio's pause snapshots (`/workspace` and
  `~/.claude`, as tar streams) come back to the Mac this way, and go out
  again on resume. smolvm's `exec` must always get `-i`: without it, output
  is held back and cut at 11 MiB without warning.
- **Redirected flows**: git and credentialed TLS, back to the Mac's `serve`.
- **The repo's objects**: a bottle's checkout borrows the repo's objects
  from a read-only mount at `/mnt/repo/objects` (an alternates file). The
  remote can't mount the Mac's repo, so it keeps a bare mirror of the
  objects, brought up to date over ssh before a machine starts, and mounts
  that at the same path. The path must stay the same: a snapshot restored
  into a new bottle finds its objects there. Pushes still go through the git
  service on the Mac, so they land in the user's repo.

## What has to keep holding

Wherever the runner is:

1. Nothing in the guest, root included, reaches the network except as the
   decider allows.
2. The runner's own loopback and link-local addresses are never reachable,
   and private addresses (RFC 1918, ULA, CGNAT) only for an allowed name or
   a credential's host. On a remote runner, "private" means the remote's
   network.
3. Which bottle a connection belongs to comes from the token smolvm was
   started with and the first machine ID seen with it, never from the guest.
   Names from first bytes are the guest's claims: a private address is
   allowed by name only if the name resolves there.
4. A credential never enters the guest, a log, or the runner. It's sent only
   over TLS that bottled verified, to the host it belongs to.
5. Git through `bottle.host` stays as folio's lanes need it, on disk in the
   user's repo (gitnamespaces(7)):
   - **Views**: `upstream` serves `refs/remotes/origin/*` as branches, plus
     tags, after fetching origin at most once a minute. `heads` serves the
     repo's own refs. `ns/N` serves only `refs/namespaces/N/`.
   - **Pushes** (`push=ns/N`) are confined to `refs/namespaces/N/`,
     append-only (no force-pushes or deletes), with one writer per
     namespace, the repo's own hooks off, and the objects in the repo's own
     store.
   - **Folio** reads `refs/namespaces/T-out/` as the agent's branches,
     writes `refs/namespaces/T-in/` for the bottle to read, and deletes both
     with the thread.

## What it means for the code now

- **One boundary, not two.** folio drives bottle through its CLI today
  (`folio/folio/engine.py`), but nobody uses bottle without folio, and that
  boundary puts the driver, broker and git exchange all on the same side. The
  boundary remote sandboxes need is control plane vs runner. So folio can
  call bottle's Python directly. The runner interface should be the one that
  stays narrow: the operations above, with a local implementation (today's
  code) and, later, an ssh one.
- **Runner work goes on the runner side.** Watching the host's memory
  pressure and having guests give memory back (docs/TODO.md) acts on
  machines, so it belongs to the runner, not to the gateway's serving half.
- **Keep `smolvm.py` the only module that runs smolvm**, and `decide` free
  of anything that needs the repo or the Keychain.

## Open questions

- **Where the runner gets its code**: the runner side (the decider and
  machine operations) is Python from this repo, so the remote needs it
  installed. Copy it over ssh on connect, or require it installed.
- **DNS**: smolvm answers guest DNS itself, from the first nameserver in the
  host's `/etc/resolv.conf`, fixed when a machine starts. On a remote runner
  that's the remote's resolver, which may not resolve names the Mac does
  (a VPN's internal names).
- **Losing the connection**: a running machine's redirected flows have
  nowhere to go. Deny them and recover when ssh is back, or stop the machine.
- **Images**: build on each runner, or build once and copy the archives.
- **Several runners**, or the Mac and a remote at once: the registry would
  record where each bottle lives.
- **smolvm on Linux** reclaims guest memory with `MADV_DONTNEED`, so the
  macOS accounting problem (smolvm#1944) doesn't apply there. Its other
  limits (TCP relay table, DAX) are the same; see docs/TODO.md.
