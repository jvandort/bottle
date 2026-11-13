# Folio

*Threads of work that outlive their sandboxes.*

[Bottle](../README.md) gives an agent a sandbox. Folio gives it a **thread**:
one line of work in one repo, with a bottle to work in, a lane in the repo for
its commits, and Claude running in the bottle, where closing your terminal
doesn't stop it.

```sh
cd ~/src/foo
folio new fix-flaky-test       # a bottle, and a lane for its commits
folio claude fix-flaky-test    # Claude, in the bottle
                               # ...close the terminal; Claude carries on
folio claude fix-flaky-test    # back to it
folio shell fix-flaky-test     # a shell beside it, in a session of its own
git fetch bottle/fix-flaky-test && git switch genie/fix-flaky-test-1
```

Folio works on the repo you're in. There's nothing to register: the first
folio command in a repo is enough.

## Threads

A thread's name is lowercase words joined by `-`, unique within its repo. Its
bottle is named `<repo key>-<thread>`, so the same thread name in two repos
never clashes. The repo key is the repo's name and a hash of its path.

- **Its bottle** is made from the repo's spec (see `folio configure`), with
  the repo checked out at `/workspace`. In it, Claude and a shell each run in a
  [shpool](https://github.com/shell-pool/shpool) session of their own, started
  the first time you attach to it. Attaching starts the bottle and the session
  if they've stopped. Closing the terminal only detaches it, and whatever is
  running carries on. shpool passes output straight through, so your terminal's
  own scrollback, mouse wheel and selection work; a reattach replays the
  session's last 10000 lines into that scrollback. A session shpool can no
  longer show (an attach that fails at once) is started again, and Claude
  carries on with its conversation. Claude starts with the `claude` feature's
  channel, so events and other bottles' messages reach its session; Claude Code
  asks you to confirm loading it each time it starts. Detach with Ctrl+Space
  Ctrl+Q, or by closing the terminal. tmux is there too, to start in one.
- **Its lane** is two git namespaces in your repo: one the agent pushes into
  and you read, one you push into and it reads. Neither side's push is ever
  refused for what the other did, so conflicts are always the agent's to
  merge. Your repo sees the lane as one remote, `bottle/<thread>`: fetching
  reads the agent's branches, and pushing hands yours over. It works whether
  or not the thread has a bottle right now. A branch you've switched to from it
  follows the agent's pushes, files and all, when nothing of yours is in the
  way (see Bottles). In the bottle, the agent's lane is
  its remote `agent` and yours is `user`, read-only. The agent can't rewrite
  or delete anything it has pushed. You can: `git push bottle/<thread>
  :refs/heads/<branch>` deletes the agent's branch too, if it's still what you
  last fetched of it, and is refused if the agent has pushed to it since.
  `curate integrate` does that for the branch it integrated. The rest stay
  until the thread is deleted.
- **Its agent** is told how to work: every branch it pushes is
  `genie/<feature>-N`, starting with `genie/<thread>-1`, and a rebase or a
  squash is pushed as the next N rather than over the old one.

A thread is **running**; **stopped**, its VM stopped with the disk intact (a
crash, a restart); **paused**, its bottle deleted and a snapshot kept; or
**missing**, its bottle gone with no snapshot, leaving only what's in its
lane. Attaching to a stopped thread starts it, and to a paused one resumes it
in a new bottle. Either way only running processes are lost.

## Notes

Every thread has notes of its own: a repo its agent keeps at `/notes` in its
bottle, for what the work teaches it -- reproducers, research, tools it built,
leads it can't follow yet. The agent commits to `main` and pushes as it goes,
and your copy, `~/.folio/<repo key>/<thread>/notes`, follows it (its `main`
tracks the agent's), so `folio notes THREAD` opens them in Obsidian, current.
They're the thread's for good: a rename takes them along, a delete leaves
them, and a new thread of the same name finds them.

## Pausing

`folio pause` snapshots a thread and deletes its bottle, so a thread you're
not working on holds no VM. The snapshot is the workspace (every branch and
stash, staged and unstaged changes, untracked files, though not ignored ones
like build output) and Claude's conversation and memory. `folio claude` or
`folio shell` resumes it, in a new bottle made from the repo's spec as it is
then, so a resume is also how a thread gets features added since.

## Runners

A thread's bottle can run on another machine of yours, a **runner** added with
`folio runner add`: its sessions, lane, notes and pause work as here, over
ssh. A bottle on one is a **drone**; your Mac stays the **controller**: your
credentials and your repos are here, and what a drone needs of them comes
here. A repo's threads are made on the repo's runner
(`folio configure --runner desktop`), else here; a thread can have its
own (`folio new fix-it --runner desktop`). It applies whenever the thread's
bottle is made, so moving a thread is pausing it and resuming it where it's
to go: `folio pause fix-it`, then `folio claude fix-it --runner desktop`. Its
snapshot comes back here and goes out again, so it resumes whatever the
runner's CPU. A thread with a bottle can't be given another runner: pause it
first. `folio list` shows where each thread runs, or will.

A drone's Claude gets its credential through this machine, so it can't reach
Claude while this machine is asleep or can't reach the runner.

A runner needs:

- Linux, with `/dev/kvm` usable by the user folio logs in as (the `kvm` group)
- x86_64 or arm64
- git
- ssh from your Mac without a prompt: a key whose passphrase is in your
  Keychain (`ssh-add --apple-use-keychain`, `UseKeychain yes`), since folio
  runs ssh in batch mode

Nothing else, and no root: folio copies bottle-cli there, the build for the
runner's architecture that comes with the bottle-cli it runs here (static, so
it runs on any distribution), and runs it over ssh. It's all a runner has of
bottle. (Untested on a real runner yet.)

- **`folio runner add RUNNER --ssh DESTINATION`** checks a machine and adds
  it under that name. `DESTINATION` is what you'd give ssh: a `Host` from
  `~/.ssh/config`, or `user@host`. It says what the machine lacks that folio
  can't set up itself, and copies bottle-cli over. Before each use of the
  runner, what differs of it is copied again, file by file: each file's
  sha256, from the manifest bottle-cli's build wrote beside it, against what
  the runner's `sha256sum` says of its copy, and only the files that differ
  are sent.
- **`folio runner ls`** lists them; **`folio runner check RUNNER`** says
  what one is and lacks; **`folio runner ssh RUNNER [COMMAND]`** is ssh there,
  on the connection folio shares for it.
- **`folio runner detach RUNNER [--offline]`** is for turning a runner off:
  it names the machines running there and asks before stopping them (in a
  terminal; told no, or with no one to ask, it stops nothing and leaves the
  runner attached, so you can have their agents wrap up first), and lets it
  go, then says it's safe to turn off. Until it's attached again, nothing
  asks it anything: `folio list` shows its bottles as `detached`, at once.
  Using one of them again (`folio claude`, `shell`, and the like) attaches it
  again if it answers, and says it's still off if it doesn't. `--offline`
  marks one that's off already, unasked.
- **`folio runner attach RUNNER`** attaches a detached runner again, once it
  answers, and names the machines detaching it stopped.
- **`folio runner rm RUNNER`** forgets a runner folio has no bottles on.
  Nothing on the machine is touched.

Runners are listed in `~/.folio/runners.json`. On a runner, bottle-cli keeps
everything of its own in `~/.bottle`: its distribution (`~/.bottle/dist`),
the images built there, the machines and the HTTP cache; and folio keeps a
bare mirror there of each repo its bottles use, in `~/.folio/git`, which your
Mac pushes into as a drone is made, so its checkout borrows objects from the
runner's disk. A drone's machine is its runner's bottle-cli's, by its own
name there (`bottle ls` here lists only this machine's).

A drone works as a local bottle does, with these differences:

- **Its connections are decided on the runner**, by bottle-cli there and its
  machine's policy, so public destinations go straight from the runner, and
  "private" means the runner's network: a drone doesn't get your Mac's VPN
  routes. DNS is the runner's. The cached hosts are answered on the runner.
- **What its policy routes comes to your Mac**: its git (fetches served from
  your repo, its `origin` fetched first for `upstream`; pushes into it, under
  the same rules as a local bottle's), its credentialed requests, ssh and
  messages, to folio serve here, through folio serve's sockets on the runner
  (`~/.folio/services/KEY/`, KEY naming this Mac) that ssh forwards back. The
  runner never holds a credential.
- **Those forwards are one ssh connection per runner**, which folio serve
  keeps from the first of its bottles there it answers to the last (its log:
  `~/.folio/logs/forward-RUNNER.log`). If it drops, the drones keep their
  public network, while what's routed is refused until it's back: it connects
  again, waiting up to a minute between tries.

A runner that's off is unreachable, not broken: folio shows its bottles as
`unreachable`, a command for one fails at once, saying so, and nothing else
waits on it.

On a Mac, ssh's "No route to host" for a machine on your network is often
macOS refusing the local network to the process that asked: give the app you
run folio from the Local Network permission (System Settings > Privacy &
Security > Local Network). folio serve has the permission of whatever started
it.

## Sidecars

A thread can have a sidecar: a second bottle, on a runner of its own (a bigger
machine, say), with a spec of its own, for the jobs that want more than the
thread's bottle has. The thread's agent reaches it as `ssh sidecar` -- a shell,
a command, scp, git over ssh -- and is told so.

```sh
folio sidecar configure fix-it --runner desktop --feature jvm:version=21 --memory 32G
folio sidecar configure fix-it --memory 64G --disk 100G --no-feature jvm
folio sidecar configure fix-it   # shows it
folio sidecar remove fix-it
folio sidecar shell fix-it     # a shell of your own there, while the thread runs
```

`folio sidecar configure` takes what `folio configure` does (below), for the
sidecar: a thread without one is given one, which needs `--runner`, and
starts with no features and 2G of memory; `folio sidecar remove` takes it away.

It has the thread's repos, as they were when it was made, and their remotes,
pushes included: what the agent pushes from there lands in the thread's lane,
beside what it pushes from the thread's bottle. Two writers share the lane as
two people share a remote: a push that lost a race is refused, never lost, and
the agent is told to fetch, merge and push again. It's
scratch: it's made just before the thread's bottle, started with it, and
deleted with it, a pause included, which keeps nothing of it: what's worth
keeping there is pushed before. So a thread that has a bottle gets its new
sidecar, or loses its old one, when it's paused and resumed. `folio list`
shows each thread's sidecar's runner, and its state (`running`, `stopped`, or
`detached` while its runner is turned off: see `folio runner detach`).

A sidecar's runner that's off holds nothing up. `folio claude` and `folio
shell` go on without the sidecar, saying so. A pause leaves it there, to be
deleted when the thread next gets one; a resume makes the thread's bottle
without one, saying so, and it comes with the bottle after, once its runner is
back. The same goes for a sidecar that can't be made for any other reason.

## The broker

`folio broker` attaches to the broker: one thread outside your repos, whose
agent talks to your threads' agents for you -- gives them work, follows it up,
relays what comes back -- and keeps a long-term memory of it all, as notes.

```sh
folio broker            # Claude, in the broker (starting it the first time)
folio broker shell      # a shell beside it
folio broker pause      # as folio pause
folio broker notes      # its notes, in Obsidian
```

`folio broker notes` adds the vault to Obsidian's list of vaults the first time
(asking to restart Obsidian if it's running, since Obsidian only reads the list
when it starts), and opens it. Obsidian has no documented way to add a vault,
so this is the undocumented one: its `obsidian.json`.

It's a thread like any other, but with no repo of yours: its bottle mounts only
its notes, at `/notes`, kept as any thread's are (see Notes), in
`~/.folio/broker-<hash>/broker/notes`. It commits them on `main` and pushes as
it goes, so the vault Obsidian opens is always its latest. Hand it a branch
the way you would any thread's notes: push it to `bottle` from the vault.

A broker from before kept its vault at `~/.folio/repos/broker`, mounted at
`/workspace`. folio moves it -- history, lane and all -- the next time the
broker has no bottle: at `folio broker pause`, or when it starts one.

Its bottle, `broker`, is made folio's way, not from a repo's spec: `claude` and
`tools`, 2G, and two permissions no thread has: `list-bottles`, to see the
running threads, and `message-any`, to send them messages. Threads can only
answer it. It can't start, stop or pause anything: a paused thread is out of
its reach until you resume it.

## Credentials

Set once, for every bottle. Features declare the credentials they need (the
`claude` feature needs `claude`, a Claude subscription token; the `github`
feature needs `github`, a personal access token; the `teamcity` feature needs
`teamcity`, an access token; the `develocity` feature needs `develocity`, an
access key, which it reads from Gradle's own key file when that has one). `folio auth set NAME`
stores one in the macOS Keychain, read from stdin, or asked for on a terminal:
claude's is what `claude setup-token` prints, run anywhere.

A credential never enters a bottle. folio serve keeps it, and attaches it as a header to the bottle's requests to the hosts the
feature named — `claude` names `api.anthropic.com`, `github` names `github.com`
and its API, `teamcity` and `develocity` name their `server`. The agent can spend the credential
against those hosts and can't read it, so an agent that sends everything it
holds to a stranger sends no token.

Spending it is not a free-for-all, though. Some requests would turn the
credential into one the bottle could hold: a token minted from it, an access
key provisioned, an SSH key or a deploy key added, a runner registered, a
codespace started. Each feature lists those routes (`forbid`), and folio serve
refuses them outright, with the credential or without. Beyond those, each
attaches its credential only to the routes its tools were seen to use
(`allow`): `github` to reads (gh's REST reads and GraphQL, clones and
fetches), so no push and no REST write; `claude` to Claude Code's own
requests. A refused request comes back 403, saying why and that only the user
can allow it, as JSON to a client that asked for JSON, so gh and Claude Code
show it. A feature's `forbid` is also an assertion over its `allow`: an
allowed route that could reach a forbidden one fails the feature's loading,
as allowed too widely. GraphQL is one route, `POST /graphql`, so a mutation through it isn't
refused, so still give each credential no more than a bottle should have: a
fine-grained GitHub token without Workflows or Administration (or read-only),
a Develocity user without administrative permissions.

Tools in the bottle talk to those hosts over HTTPS as usual. TLS is opaque, so
for those hosts only, the bottle's machine routes the connection to folio serve:
bottle-cli, which carries the machine's connections, completes the bottle's TLS
handshake with a certificate from the machine's **CA**, and gives folio serve the
plain request, after a PROXY protocol header saying where it was going and
from which machine. folio serve attaches the credential and makes its own verified
HTTPS connection to the server. Every other connection goes straight to where
it was headed, untouched. The host is also reachable when it's a private
address, since configuring it is what naming it means; nothing else about that
host opens up.

The machine's CA, and what it may answer for, are bottle-cli's: see bottle's README, "A machine's network".

A CLI doesn't know its token is being attached for it, and usually won't make a
request until it thinks it's logged in, so a bottle gets a **stand-in**: a fake
token, never a secret. A feature either writes one itself (`teamcity` puts one
in the CLI's config file) or names an environment variable for bottle to set in
every bottle with that feature (`claude` names `CLAUDE_CODE_OAUTH_TOKEN`).

A bottle whose features need a credential that isn't set says how to set it,
and goes on without: its requests reach the host unauthenticated.

```sh
claude setup-token | folio auth set claude
folio auth set teamcity      # asks for it
folio auth list              # every credential features declare, and which are set
folio auth rm teamcity
```

Bottles pick up a change when they next start.

## Features

A bottle's image is built with its features when it's made, if it isn't built
yet. Each image records what it was built from, so an image whose features (or
the base underneath) have changed since is stale: the next bottle rebuilds it,
and after any build bottle deletes its stale images that no bottle uses. Each
runner keeps its own images, built for its own CPU.

Features, in the [Dev Container feature](https://containers.dev/implementors/features/)
format (a `devcontainer-feature.json` and an `install.sh` per directory):

- `tools`: a minimal working environment (ripgrep, fd, jq, curl, vim, tmux, shpool, ...).
- `jvm`: Eclipse Temurin JDKs, from Adoptium's signed apt repository. Options:
  `version` (default `25`), the default `java` and `JAVA_HOME`; and
  `additionalVersions`, more JDKs alongside it, e.g. `17,21`. Every JDK is in
  `/usr/lib/jvm` (also as `/usr/lib/jvm/jdk-<version>`), where tools like
  Gradle's toolchains find them.
- `python`: Debian's Python 3, with `venv`, `pip`, and `python` as a name for
  it. `pip install` works without a virtualenv: Debian's PEP 668 marker is
  removed, since a bottle is disposable and a new one undoes whatever an
  install breaks. Option: `externallyManaged` (default `false`) keeps the
  marker, and Debian's behaviour, if you'd rather work in a venv.
- `cc`: gcc as `cc`, with the C library headers (`libc6-dev`, which gcc only
  recommends and every link needs): what cargo needs to link build scripts,
  even when the output is wasm. No g++ or make.
- `rust`: Rust through rustup 1.29.2 (checked against its checksum): a
  toolchain with clippy and rustfmt, in a shared `RUSTUP_HOME`
  (`/usr/local/rustup`) the bottle's user owns, so `rustup target add` needs no
  sudo; `cargo`, `rustc` and `rustup` are in `/usr/local/bin`. `cargo install`
  puts programs in `~/.cargo/bin`, on login shells' PATH. Brings `cc`.
  Options: `toolchain` (default `stable`), `components` (default
  `clippy,rustfmt`) and `targets` (default none), comma-separated.
- `chromium`: Playwright for Python, with the headless Chromium build it's
  made for (`playwright install --with-deps --only-shell chromium`, at
  `/opt/ms-playwright`, in the image), to see a built web page actually
  render; Chromium's own `--screenshot` captures before scripts have run.
  Brings `python`. Option: `playwright` (default `1.64.1`); its Chromium comes
  with it.
- `claude`: Claude Code, from Anthropic's signed apt repository, ready to work:
  every directory is trusted (the bottle is the sandbox), first-run setup is done, it reaches the
  API through folio serve, which holds the `claude` credential
  (`folio auth set claude`) so the agent can spend its own subscription
  token without ever being able to read it, and it reads
  bottle's context for the agent (`~/BOTTLE.md`) as its instructions. Sessions
  start on Opus 5.5 at medium effort. It also
  has a channel, `bottle-event`, an MCP server that brings events into a
  running session (`bottle exec MACHINE -- bottle-event send -f FROM TEXT`, which
  waits until the session takes it) and gives the agent a `send` tool for
  messages to other bottles: folio serve delivers each one as an event whose `from`
  is an address for answering the sender, issued to the recipient and good
  from no other bottle, and answers once that session has taken it (404 for no
  running bottle the sender may reach that way, 504 for no session that took it
  in time). Only a bottle with the `message-any` permission (see its spec, below)
  can start a conversation, by a bottle's name; any other can only answer.
  Claude Code
  runs the channel when started with `--dangerously-load-development-channels
  server:events`, as folio starts it. Options:
  `permissionMode` (default `bypassPermissions`: the bottle is the sandbox),
  `theme` (default `dark`), and `tui` (default `default`: `fullscreen` would
  capture the mouse, and a bottle has no clipboard to copy to instead).
- `github`: GitHub's CLI, `gh`, from GitHub's signed apt repository, with `gh`
  and git both authenticated to github.com by folio serve, which holds the
  `github` credential. `folio auth set github` stores a
  [fine-grained personal access token](https://github.com/settings/personal-access-tokens):
  what the token may do is what a bottle may do on GitHub, and a push there
  doesn't pass through your repo the way a push to a bottle's remote does, so give
  it just the repositories and permissions the agent needs. GitHub's ssh
  remotes (`git@github.com:...`) are fetched over https instead, since a bottle
  has no ssh key.

  ```sh
  folio configure --feature github
  ```
- `teamcity`: JetBrains' [TeamCity CLI](https://www.jetbrains.com/help/teamcity/teamcity-cli.html),
  verified against JetBrains' signature from its GitHub release, with its access token
  held by folio serve rather than the bottle (see Credentials).
  Options: `server` (required), the TeamCity server, e.g.
  `https://teamcity.example.com`; `version`, the CLI release (default `latest`); and `readOnly`
  (default `true`), which blocks everything but GET, so a bottle can read
  builds but not start or change anything; pass `readOnly=false` to let it
  start them. A token with read-only permissions is the real boundary.

  ```sh
  folio configure --feature teamcity:server=https://teamcity.example.com
  ```
- `develocity`: [Develocity's MCP server](https://docs.develocity.ai/2026.3/integrations/agentic-ai/mcp-servers/ai-agents/)
  for Claude Code, with its access key held by folio serve rather than
  the bottle. The key is read on the machine from Gradle's
  `~/.gradle/develocity/keys.properties` (`server=key` lines, as
  `./gradlew provisionDevelocityAccessKey` writes them) each time the bottle
  starts, so there's nothing to log in to; a server the file has no line for
  uses the one `folio auth set develocity` stored instead. Develocity serves MCP itself, so
  nothing is installed: the server goes in Claude Code's user config, which
  the feature writes whether or not `claude` is in the spec (it installs after
  it when it is). The server's API is authenticated the same way. Options:
  `server` (required), the Develocity servers, separated by commas, e.g.
  `https://develocity.example.com`, each with its own key and its own MCP
  server in Claude Code, `develocity-<host>` (`develocity-develocity-example-com`); `keyFile`, where the keys are (default
  `~/.gradle/develocity/keys.properties`). The key's user needs the "Access build
  data via the API" permission, and should have no administrative ones. The
  bottle's own `~/.gradle/develocity/keys.properties` has a line for each
  server with the placeholder in place of its key, so Gradle needs nothing set:
  folio serve puts the key where the Develocity plugin presents it, for Build Scans,
  the build cache, test distribution and predictive test selection, and refuses
  to mint tokens (`/api/auth/token`) or provision access keys. A server on a private address, as one behind a
  VPN often is, is reachable because the credential names it.

  ```sh
  folio configure --feature develocity:server=https://develocity.example.com,https://ge.example.com
  ```

A feature can take options: `FEATURE:OPTION=VALUE[,OPTION=VALUE]`, e.g.
`--feature jvm:version=21,additionalVersions=17,11`. Options left out take
their defaults.

bottle runs features itself (no Dev Container tooling or Node) and supports a
subset of the format: `id`, `version`, metadata, `options` (string and boolean),
`containerEnv`, `dependsOn` / `installsAfter` naming local features, and
`customizations.bottle` (the credentials a feature needs, the hosts each one is
attached to, the stand-in it wants and the routes it's refused, allowed or
rewritten on, `cache`, the hosts whose downloads are
kept, `apt`, the repositories and packages it installs, all features' together
in one step before their `install.sh`, `requiredOptions`, the options a user
has to set, and `context`, a sentence or two for the agent about what the
feature gives it, added to `~/BOTTLE.md` with `${option}` filled in).
Anything else in a definition is an error. Remote features aren't supported.

## Bottles

Folio makes each bottle (folio/bottle/bottles.py) from a
**spec**: its features and memory (`folio configure` sets a repo's). Its name
is its identity, and its thread's: a name that's taken is refused. Its
record is kept with its thread, in folio's home
(`~/.folio/<repo>/<thread>/bottles/<name>.json`), and it goes with its thread.

```json
{"features": ["claude", "jvm:version=21", "tools"], "memory": "8G"}
```

A spec can give the bottle's `disk`, in G (`"disk": "50G"`): the disk what
it writes goes to, sparse, so it costs what it holds; 64G if it doesn't say.
Its VM gets every core of the machine it runs on, up to 16.

A spec can also give the bottle `permissions`, what folio serve lets it do beyond
what every bottle may: `message-any`, to send messages to a running bottle by
its name rather than only answer one; and `list-bottles`, to list the other
running bottles, each one's name, repo and `labels` (which a spec can give,
`{"labels": {"thread": "fix-it"}}`; folio's say which thread a bottle is for).
folio serve checks the bottle's spec, never the bottle; the bottle sees its
permissions as `BOTTLE_PERMISSIONS`, so its tools know what to offer. Folio
never gives a thread's bottle permissions.

Each of its repos is checked out at a path of its own, any absolute path
(folio's: the thread's repo at `/workspace`, its notes at `/notes`). Its objects are mounted read-only, so nothing is
cloned and the bottle can't change the repo except by pushing to a remote
it's served. The first is its machine's workdir, where `bottle shell` and
`bottle exec` start, and where an SSH login goes. What's checked out there is
a branch (the repo's, else origin's) or a commit, by default the origin
remote's default branch, or, if there's no origin, whatever the repo has
checked out.

Each is served the remotes it's given, `NAME:fetch=VIEW[,push=ns/NAMESPACE]`, live, at
`http://bottle.host/git/<PATH>/<NAME>`, which folio serve answers. What a fetch
sees is one of three views:

- `upstream`: your repo's upstream (its `origin/*` branches), read-only.
  Fetching first fetches your repo's `origin` on your Mac, so the bottle gets
  the upstream's latest even if you haven't fetched.
- `heads`: your repo's own branches: how the agent sees what you've integrated.
- `ns/NAMESPACE`: the refs in one [git namespace](https://git-scm.com/docs/gitnamespaces)
  of your repo, and nothing else.

With `push`, pushes go into that namespace. **Your checked-out branches follow
a push**: a branch of yours whose upstream is a remote reading that namespace
(folio's `bottle/<thread>`, so any branch you got with `git switch` after
fetching it) is fast-forwarded to what the bottle pushed, files and all, in
whichever worktree has it checked out, and the remote's tracking refs are
fetched. Only when nothing would be lost: a branch with commits of yours, a
change in the way, or a worktree in the middle of a merge, rebase,
cherry-pick, revert or bisect is left alone; so is a branch checked out
nowhere, though one you're reading in curate comes along by curate's own hook:
the fetch runs with your repo's hooks, as yours would. The push succeeds either way, and its
output tells the bottle what moved and what didn't (`remote: bottle: ...`);
folio serve logs it too. `git config bottle.follow false` in your repo turns it
off. A namespace is what confines the
bottle: every ref its `receive-pack` resolves is inside it, and your own refs
aren't advertised to it at all. Pushes are append-only, refusing force-pushes
and deletes, so nothing you've already read changes underneath you; no other
bottle may push into the same namespace, but one it may ssh to; and your repo's own hooks don't run.
Reading one namespace and writing another makes a two-lane exchange, where
neither side's push is ever refused for what the other did: that's how
[folio](folio/README.md) gives each thread its lane (`origin:fetch=upstream`,
`host:fetch=heads`, `agent:fetch=ns/T-out,push=ns/T-out`, `user:fetch=ns/T-in`).
A repo given none gets `origin` (`upstream`) and `host` (`heads`), both read-only.

`~/BOTTLE.md` says what the bottle, its remotes and its features are, and,
after that, what its maker adds: folio's say how to work in a thread.

Its machine runs on a runner: `local`, this machine, by default, or one added
with `folio runner add`. The image is built there if it isn't yet, and the
spec's memory must fit that machine.

A bottle can be let ssh to another (folio's thread, to its sidecar), which must
exist already: `ssh NAME` in this one is a login there, as `genie`, and so are scp and git over ssh. That's all it
reaches: NAME's address goes to that bottle's sshd and nowhere else, never to
a machine on your network. It's the bottle as it was when the grant was made:
one made again under the same name isn't reached until this one is made again
too. Bottle doesn't start it: start both. The two may push into the same
namespaces, as two people share a branch: a push that lost a race is refused,
not lost.

bottle-cli keeps its state (machines, images, the HTTP cache) in
`~/.bottle`, and folio its own (threads, bottles' records, runners) in `~/.folio`; `BOTTLE_HOME` and `FOLIO_HOME`
name others.

## folio serve

`folio serve start|stop` starts or stops the background process that serves what
bottles' machines route to it. A machine decides each TCP connection the bottle opens
itself, by the network policy it was made with (`network.json` in its
directory): public destinations go straight there; private addresses are
refused, but for a name allowed private; `bottle.host`, ssh to other bottles
and credentials' hosts are routed to folio serve's sockets, one per service (`~/.folio/services`),
which answers `bottle.host` with the bottle's git remotes. A refused
connection is reset, and logged in the machine's `boot.log`. A bottle's names
are looked up by the machine it runs on, with its resolver, as they're asked;
a bottle has no UDP or IPv6. A machine starts with nothing but its name
(`bottle start MACHINE` works without folio serve); what it routes to folio serve is
refused while folio serve isn't running. folio serve logs one line per connection it
serves, in `~/.folio/logs/serve.log` (folio/bottle/daemon.py).

folio serve starts automatically when a command needs it, and when it starts it
picks up the bottles there are. It stops itself once there's no bottle left to
answer: when the last is deleted, or its thread paused. While it's stopped, a running bottle's
connections go on as its policy says, but for what it routes to folio serve
(`bottle.host`, ssh, credentials' hosts), which is refused. A command that finds it running other code (another
checkout's, or this one's before it changed) restarts it first, and says so.

## Commands

| Command | |
| --- | --- |
| `folio new THREAD [--from BRANCH] [--runner RUNNER]` | Start a thread: its bottle and its lane |
| `folio claude [THREAD] [--runner RUNNER]` | Attach to the thread's Claude, starting the thread if it's new |
| `folio shell [THREAD] [--runner RUNNER]` | Attach to the thread's shell, starting the thread if it's new |
| `folio pause [THREAD]` | Snapshot a thread, and delete its bottle |
| `folio notes [THREAD]` | Open a thread's notes in Obsidian |
| `folio rename THREAD NEW` | Rename a paused thread: its record, lane and remote (its branches keep their names) |
| `folio list` (`ls`, `status`) | The repo's threads, most recently active first, where each runs, its sidecar's state, and the space a paused one's snapshot takes up |
| `folio delete [THREAD] [--yes]` | Delete a thread: its bottle, its lane and its remote |
| `folio configure [--feature F] [--no-feature F] [--memory M] [--disk D \| --no-disk] [--runner R \| --no-runner]` | The repo's spec, its threads' features, memory and disk, and its runner: shown without options |
| `folio sidecar configure` / `remove` / `shell [THREAD]` | A thread's sidecar (see Sidecars) |
| `folio broker [claude\|shell\|pause\|notes]` | The broker: attach, pause, or open its notes |
| `folio repo list` (`ls`) | Every repo folio has state for |
| `folio runner add` / `ls` / `check` / `ssh` / `detach` / `attach` / `rm` | The other machines folio makes bottles on (see Runners) |
| `folio auth set` / `list` / `rm` | Credentials bottles get (see Credentials) |
| `folio serve start` / `stop` | What bottles' machines route to folio: their git, credentials, ssh and messages; it stops itself once every thread is paused |

Leave the thread out, on a terminal, and you choose it from a list: the
repo's threads, most recently active first, with their state. Move with j/k
or the arrow keys, Enter to choose, q or Esc to leave. For `claude` and
`shell` the list starts with a new thread, which asks for its name.

**`folio new`** starts from origin's default branch, or `--from BRANCH`.
`folio claude` and `folio shell` given a thread that doesn't exist start it
as `folio new` would, from origin's default branch, and attach to it.

**`folio delete`** says first what deleting would lose: branches in the lane
that aren't on your branches or origin's, and work in the bottle that never
reached the lane. Then it asks you to type the thread's name. Without a
terminal it needs `--yes`.

**`folio configure`** shows the repo's spec, which every new bottle of its
threads is made from, and changes it: `--feature` adds a feature (one there
already gets the options given), `--no-feature` takes one away, by its name
alone, `--memory` and `--disk` set them (`--no-disk`: the default again), and
`--runner` sets where its threads' bottles are made (see Runners;
`--no-runner`: this machine again). Bottle checks the spec
before folio saves it: that it's valid, while whether its memory fits is
checked against the runner each bottle is made on. A repo's spec can't give `permissions`
or `labels`: folio labels each thread's bottle with its thread itself, and never
gives it permissions. Every repo's spec has `tools`, which has the shpool
every thread's sessions run in: it can't be taken away, and a spec saved
without it gets it. A repo with no spec gets just that, and 2G of memory; add
`claude` for Claude.

```sh
folio configure --feature claude --feature jvm:version=21 --memory 8G --disk 50G
folio configure --no-feature jvm
```

## Development

```sh
cd folio && python3 -m unittest discover -s tests -t .
cargo test -p folio-cli
```

folio is becoming a Rust program, folio-cli (`cli/`, in bottle's Cargo
workspace), which folio's Python is a front end for, as bottle's Python was
for bottle-cli: so far, runners (`folio runner`, `cli/src/runner.rs`). It
runs bottle's code only by running bottle-cli, never by linking bottle's
crates (`cli/tests/boundary.rs` checks).

Folio makes its bottles with bottle's code, in its own process (`folio/engine.py`,
calling `folio/bottle/bottles.py`). The tests stand `tests/fake_bottle.py` in for it, a
bottle CLI that keeps its bottles in a JSON file and runs commands in a real
clone of the repo, so they need no VM: `$FOLIO_ENGINE` names `tests/cli_engine.py`,
which drives it.

`tests/bottle/` tests the code folio makes and serves bottles with
(`folio/bottle/`). On a Mac with bottle-cli built (`cargo xtask dist`, or
`BOTTLE_CLI` naming one), that includes two suites that run real machines,
and take minutes the first time, while they pull and build images:

- `tests/bottle/test_krun_contract.py`: what folio relies on bottle-cli for.
  Run it after changing bottle-cli, the agent or libkrun.
- `tests/bottle/test_end_to_end.py`: a real bottle, made as folio makes one and
  run in through `bin/bottle`, in its own `BOTTLE_HOME` (`BOTTLE_E2E_HOME`,
  default `/tmp/bottle-e2e`).

Elsewhere they're skipped. Runners are tested here with a fake ssh, which runs
each command on this machine, in a home of its own: `tests/bottle/test_drone.py`
is a drone end to end, with only ssh and bottle-cli faked.
