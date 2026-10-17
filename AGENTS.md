# AGENTS.md

Instructions for coding agents. Part 1 is for when someone asks you to
install braid in a project and set it up for agents. Part 2 is for working
on braid itself, in this repository.

Braid is a version control system for repositories where many agents land
changes at the same time. Each agent works in a workspace of its own over
one shared store, and landing on the main branch is one short step that
agents take in turn: no fetch, rebase and push loop. Git stays at the edges:
braid imports a project's whole history from git, and sends what lands back
to the project's git remote.

**Braid is pre-alpha.** Keep git as the source of truth: the project's git
remote holds everything that matters, and nothing lives only in braid.

## 1. Installing braid and setting it up in a project

### Install

Braid builds from source with cargo. It needs Rust 1.89 or newer (from
<https://rustup.rs>) and git.

```bash
cargo install --locked --git https://github.com/KikoCis/elffuss-braid elffuss-braid-cli
```

This puts a `braid` binary in cargo's bin directory, `~/.cargo/bin` unless
`CARGO_HOME` says otherwise, which must be on `PATH`. Running the same
command again upgrades it.

### Is it installed, and set up here?

```bash
braid --version     # prints "braid" and its version once it is installed
braid refs          # in the project root: its branches, once it is set up
```

Before it is installed, `braid --version` fails with "command not found"
(or cargo's bin directory is not on `PATH`). In a project where braid is
not set up, `braid refs` fails with "not inside a braid repository".

### Set up a project

Do this once, in the root of the project's git checkout: on its main
branch, with nothing to commit (`git status`) and nothing new to pull
(`git pull --ff-only`). The commands say `main`; if the project's main
branch has another name, such as `master`, write that instead.

```bash
braid init                       # creates .braid/, for the store and the agents' workspaces
echo '/.braid/' >> "$(git rev-parse --git-path info/exclude)"
printf 'name = %s\nemail = %s\n' "$(git config user.name)" "$(git config user.email)" > .braid/config
braid git import .               # the project's whole history: every local branch and tag
braid agents init                # the rules for agents, in AGENTS.md
```

- The second line makes git ignore `.braid/` in this checkout only, so that
  nothing of braid's is ever committed with git.
- `.braid/config` names who signs the changesets agents make, here the
  same person as git's commits. If git has no `user.name` or `user.email`,
  write them in by hand; otherwise every session has to set `BRAID_AUTHOR`.
- `braid agents init` writes a section into `AGENTS.md`, creating the file
  if there is none, between a `<!-- braid:start -->` line and a
  `<!-- braid:end -->` line. The section tells every agent session to work
  in a workspace of its own and to land through braid, and names the branch
  agents land on: `main`, `master`, or the project's only branch.
  `--branch <name>` names another, and `--dry-run` shows what it would
  write. Text outside the markers is never touched, and running it again
  rewrites the section in place.
- If the project has a `CLAUDE.md`, `braid agents init` adds the line
  `@AGENTS.md` to it, so that Claude Code reads the section too. If agents
  work on the project with Claude Code and there is no `CLAUDE.md`, run
  `touch CLAUDE.md` before `braid agents init`.

Commit what `braid agents init` wrote, so that every workspace and every
clone has the rules, then import that commit too:

```bash
git add AGENTS.md
test -f CLAUDE.md && git add CLAUDE.md
git commit -m "Set up braid for agents"
braid git import .               # braid's main takes the commit
braid log main -n 1              # and shows it
```

### Every agent session, from then on

The section in the project's `AGENTS.md` tells each session what to do. In
short:

- **Never edit files, `git add`, `git commit` or `git push` in the project
  root.** It is the shared checkout. Every session works in a workspace of
  its own, under `.braid/work/`.
- **Start** from the project root with a workspace named for the session:
  1 to 64 letters, digits, `.`, `_` or `-`. `braid workspace list` shows the
  names taken, and a name that is taken is refused. Then edit, build and
  test only in that directory. A workspace is a whole checkout of the
  branch, so builds in it start from scratch.

  ```bash
  braid workspace add agent-1 .braid/work/agent-1 --track main
  cd .braid/work/agent-1
  ```

- **Use braid, not git, in the workspace.** It has no `.git` of its own, so
  `git` there finds the shared checkout's repository and knows nothing of
  the session's work. `braid --agent agent-1 status` says what changed.
- **Record and land** from the workspace. `--agent` (or `BRAID_AGENT`)
  names the session in the op log and in every changeset it makes; pass it
  every time, since a harness may not keep variables between commands. If
  the shell does not keep its directory either, add `--workspace agent-1`
  and run the commands from the project root.

  ```bash
  braid --agent agent-1 status --json
  braid --agent agent-1 commit -m "Say what changed and why"
  braid --agent agent-1 land --json
  ```

  There is no staging area: `commit` records every change in the
  workspace. `land` puts the workspace's changesets on `main`, as a
  fast-forward or rebased onto whatever landed first.
- **Before a new task**, `braid --agent agent-1 update` brings the
  workspace onto `main`. `status --json` says how far behind it is
  (`behind_by`).
- **Done**: once everything has landed, `braid workspace remove agent-1`
  from the project root. It refuses while the workspace holds work that
  has not landed.
- Every command takes `--json`. Changesets are signed by `BRAID_AUTHOR`
  (`"Name <email>"`), or else by the `name` and `email` in `.braid/config`.
- New files that look like secrets (`.env*`, `*.pem`, `id_*`) or are larger
  than 1 MiB are held back until `braid track <path>`: `commit` refuses,
  naming them, and `--force` leaves them out.

### Conflicts

When another agent landed a change to the same lines first, `land` exits
with status 2 and leaves `main` where it was. Exit status 2 also means a
command line braid could not parse, so check the JSON: only a conflict
prints `"outcome": "conflicts"`. Each conflict names its sides: `base`,
`branch` (what `main` holds now) and `workspace` (the session's own
change), and `next` says what to run.

```bash
braid --agent agent-1 update      # exit status 2: conflict markers in the files
# edit the files until no marker is left; `braid status` lists what still blocks
braid --agent agent-1 commit      # keeps the change's message
braid --agent agent-1 land --json
```

`braid resolve <path>` settles conflicts that are not about content, such
as a file deleted on one side and changed on the other.

### Keeping the git remote up to date

Landings change braid's `main`, not git's. `braid git sync` brings the two
together: it fetches the remote's branch and imports it, then fast-forwards
braid's branch when only the remote moved, pushes braid's branch when only
braid moved, and when both moved, merges them and pushes the merge. The
push is a fast-forward with a lease on what was just fetched, so the
remote's history is never rewritten; braid never force-pushes. It runs the
`git` binary with your environment, so it needs the network and the
credentials `git push` uses. Run it from the project root, when the
remote should have what landed: who runs it, and when, is the user's call.

```bash
braid git sync "$(git remote get-url origin)" --branch main --dry-run --json
braid git sync "$(git remote get-url origin)" --branch main
git pull --ff-only               # the shared checkout follows
```

- With `--json`, `outcome` is one of `up_to_date`, `created`,
  `fast_forwarded`, `pushed`, `merged`, `conflicts`, `remote_moved` or
  `rejected`. `--dry-run` writes nothing.
- Each changeset reaches git as a commit of its own, with an `agent` header
  that names the agent that made it.
- Exit status 1 with `remote_moved`: someone pushed meanwhile; sync again.
  With `rejected`, also exit status 1, the remote refused the push, as a
  protected branch does.
- Exit status 2 with `conflicts`: braid and the remote changed the same
  lines. Nothing moved and nothing was pushed. Merge them in git, in the
  shared checkout, and sync again:

  ```bash
  git init --bare .braid/export.git          # a scratch repository, which git ignores
  braid git export .braid/export.git --ref main
  git fetch .braid/export.git main
  git merge --ff-only FETCH_HEAD             # the shared checkout takes braid's main
  git fetch origin main
  git merge FETCH_HEAD                       # the remote's: fix the conflicts, then git commit
  git push
  braid git sync "$(git remote get-url origin)" --branch main
  rm -rf .braid/export.git
  ```

  Braid knows the commits it exported, so the sync after the push
  fast-forwards braid's `main` to the merge. Merge in this order, with
  braid's branch as the first parent: braid follows a branch's first
  parents to tell what a workspace has landed, so the workspaces carry on
  from the merge.

No remote? Any bare repository will do: `git init --bare ../project.git`,
then `git remote add origin ../project.git` and `git push -u origin main`
in the shared checkout.

### Undoing

- `braid op log` lists every operation (imports, landings, syncs), newest
  first, with the agent that made it. `braid undo` reverts the newest one
  that is not undone yet, whoever made it, so read the log first. A landing
  undone leaves its changeset in the workspace that made it, ready to land
  again.
- A change that landed under other changes cannot be undone that way.
  There is no revert command yet: land a change that puts the files back.
- To take braid out of a project: land what the workspaces hold and sync
  it, remove each workspace (`braid workspace remove <name>`, which refuses
  while work has not landed), delete `.braid/`, the `/.braid/` line of
  `.git/info/exclude`, the braid section of `AGENTS.md` (its two marker
  lines and everything between them) and the `@AGENTS.md` line it added to
  `CLAUDE.md`, and commit that with git. Then
  `cargo uninstall elffuss-braid-cli` removes the command.

### What not to do

- Do not edit files, `git add`, `git commit`, `git push` or `git pull` in
  the shared checkout while you work: only setting up, syncing and merging
  a sync conflict touch it, as above.
- Do not share a workspace between sessions, or work in another session's
  workspace.
- Do not run `braid undo` without reading `braid op log` first.
- Do not `braid git export` into the shared checkout or into the remote: it
  overwrites git refs of the same names. `braid git sync` is the way to the
  remote.
- Do not `commit --force` past conflict markers, and never `braid track` a
  secret.
- Do not delete `.braid/` while a workspace holds work that has not landed
  and been synced: the workspaces and their changesets live there.
- Do not force-push the branch braid syncs with, and do not keep anything
  only in braid.

## 2. Working on braid itself

Braid is written in Rust, edition 2024, and needs Rust 1.89 or newer. CI
pins the toolchain to 1.99.0.

| Path | What it is |
|---|---|
| `crates/format` | Object ids and the hashed format (frozen; see `docs/spec/format-v1.md`). No I/O. |
| `crates/engine` | Change lists, merges, landing, ancestry and blame. No I/O. |
| `crates/store` | The store: objects in shared segments, refs in the op log. |
| `crates/git` | The git bridge, through gitoxide. |
| `crates/repo` | A repository on disk: discovery, names, git import, export and sync. |
| `crates/wc` | Workspaces: checkouts, status, commit, land and update. |
| `crates/cli` | The `braid` command; `braid agents init` is `crates/cli/src/agents.rs`. |
| `crates/sim` | Deterministic simulation of many agents landing at once. |
| `docs/` | The design and the format specification. |
| `site/` | braid.elffuss.com, in English and Spanish; `site/agents.md` is part 1 of this file. |

### Build and test

```bash
cargo build
cargo test --workspace
```

### Before every commit

These are the checks CI runs. All of them must pass:

```bash
cargo fmt --all
cargo +1.99.0 clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
cargo +1.89 check --workspace --all-targets
scripts/privacy-check.sh
```

### Rules

From [CONTRIBUTING.md](CONTRIBUTING.md), in short:

- No code from git or any other GPL project: braid is Apache-2.0. The git
  bridge uses gitoxide. Dependencies must be compatible with Apache-2.0
  (`deny.toml` lists the licences accepted).
- The hashed formats (`crates/format`, sections 1 to 5 of the spec) are
  frozen. Changing the bytes of an id, a tree or a changeset needs a new
  format version, with the spec and its golden vectors updated in the same
  change.
- Tests come with the change, and a bug fix comes with a test that fails
  without it.
- Performance claims are ratios against git on the same machine, with the
  script that reproduces them (`bench/run.py`). Never publish absolute
  timings or describe the hardware.
- Every source file starts with `SPDX-License-Identifier: Apache-2.0` and
  `Copyright (c) 2026 UtopiaIA` in a comment.
- Code and documentation are in English. The site has every text in English
  and in Spanish (see `site/README.md`).
- The repository is public: no absolute paths, user names, real e-mail
  addresses (use `example.org`), hardware or timings in anything committed.
- Part 1 of this file and `site/agents.md` say the same thing, and a test
  checks it: change both together.
