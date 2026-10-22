# TODO

- **A spec without `claude` fails confusingly.** `folio claude` then gets a
  plain shell: the session's command exits at once, and shpool starts a shell
  in its place when it's attached to. Say that the repo's spec has no
  `claude`, and how to add it.
- **A resumed workspace keeps its old remotes.** `.git/config` comes back from
  the snapshot, so a thread paused before its remotes changed (`work`, before
  `agent` and `user`) resumes with the old ones, and the new bottle serves
  the new ones. Re-add the bottle's remotes after a restore.
- **Bottle's own hints show through folio.** `bottle new`'s stdout reaches the
  terminal, so a resume ends with bottle's "Open a shell with: bottle shell
  <bottle>", the wrong advice inside folio. Silencing stdout would also hide a
  login's URL (`claude setup-token` prints it there), so it wants a quiet mode
  in bottle that keeps a login's output.
- **Make `bottle new` and `delete` faster.** Timed from bottle.log, with the
  image built, on apple/container (retime on smolvm, which has no anchor):
  `new` 4.0s, `delete` 2.1s, `pause` 1.6s before its delete. Of that:
  - **~0.3s per `exec`.** `new` makes four in setting up (contract, CA,
    bottle.host, workspace); `pause` five (list-panes, kill-server, two archives,
    work.txt). Fold them where the output allows.
  - **The workspace's first fetch** (part of 1.2s with the CA and egress) has
    the host `git fetch origin` before serving origin. A new bottle could
    serve what the repo has and leave fetching to the agent. Measure first.
- **What's in a thread's lane, in `folio list`.** How far the agent's branches
  are ahead of your copy, and whether you've commits it hasn't seen. Both
  directions, because the loop has two.
- `--git-repo` with a URL, and folio on a different machine from its sandboxes.
  The design keeps both possible: no writable mounts, and everything over exec.
