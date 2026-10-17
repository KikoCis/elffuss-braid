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

Read this part whole before you run anything, and run its commands exactly
as they are written.

### Install

Braid builds from source with cargo. It needs:

- Rust 1.89 or newer (`rustc --version` says which; <https://rustup.rs>
  installs it), a C compiler (`cc`), git, and a POSIX shell such as bash:
  every command here is written for one.
- Linux, macOS or another Unix-like system. Windows is not tested yet: use
  WSL there.
- The network, and permission to write in cargo's directory, `~/.cargo`.
  A sandboxed harness blocks both by default: ask the user to allow them,
  or to run the command themselves.

```bash
cargo install --locked --git https://github.com/KikoCis/elffuss-braid --tag v0.0.2 elffuss-braid-cli
```

It compiles braid and its dependencies from source, which takes a while:
give the command a long timeout, or run it in the background, since a
command cut short starts again from the beginning. It puts a `braid` binary
in cargo's bin directory, `~/.cargo/bin` unless `CARGO_HOME` says
otherwise. If that directory is not on `PATH`, call braid as
`"$HOME/.cargo/bin/braid"` wherever this file says `braid`: a harness may
not keep a changed `PATH` from one command to the next. Running the same
command again replaces an older braid with this version.

### Is it installed, and set up here?

```bash
braid --version     # "braid 0.0.2", or newer, once it is installed
braid refs          # in the project root: its branches, once it is set up
```

- Before braid is installed, `braid --version` fails with "command not
  found", as it does while cargo's bin directory is not on `PATH`. An older
  braid, or one that says `agents` is not a subcommand, needs the install
  command again.
- In a project where braid is not set up, `braid refs` fails with "not
  inside a braid repository". Where it lists the project's branches and
  `AGENTS.md` has a braid section (a `<!-- braid:start -->` line), braid is
  set up: skip to "Every agent session, from then on".

### Set up a project

First read the project's `AGENTS.md`, `CLAUDE.md` and the like for rules
about git: branches, pull requests, how to commit. Look too for what braid
does not run: git hooks (in `.git/hooks`, or where
`git config core.hooksPath` says), Git LFS (`filter=lfs` in
`.gitattributes`: braid sees the pointer files, not the content) and a main
branch that takes changes only through pull requests. Tell the user what
you found, and if any of it clashes with agents landing on the main
branch, ask how to go on before you set anything up.

Then work in the root of the project's git checkout, on its main branch,
with nothing to commit (`git status`) and nothing new to pull
(`git pull --ff-only`; skip it when the branch has no upstream). In a
shallow clone, run `git fetch --unshallow` first: braid imports the whole
history. The commands say `main`; if the project's main branch has another
name, such as `master`, write that instead. Every line is safe to run
again, so after a failure, start again from the top.

```bash
test -d .braid || braid init     # .braid/: the store and the agents' workspaces
grep -qsxF .braid "$(git rev-parse --git-path info/exclude)" || echo .braid >> "$(git rev-parse --git-path info/exclude)"
test -f .braid/config || printf 'name = %s\nemail = %s\n' "$(git config user.name)" "$(git config user.email)" > .braid/config
touch CLAUDE.md                  # for Claude Code, which reads CLAUDE.md, not AGENTS.md
braid git import .               # the project's whole history: every local branch and tag
braid agents init                # the rules for agents, in AGENTS.md
```

- The second line makes git ignore every `.braid` in this checkout, and
  only in this one: braid's directory, and the `.braid` file that marks
  each workspace, so that nothing of braid's is ever committed with git.
- `.braid/config` names who signs the changesets agents make, here the
  same person as git's commits. If git has no `user.name` or `user.email`,
  write them into it by hand; otherwise every session has to set
  `BRAID_AUTHOR`.
- Most harnesses read `AGENTS.md`. Claude Code reads `CLAUDE.md` instead,
  and Gemini CLI `GEMINI.md`: `braid agents init` adds the line
  `@AGENTS.md` to each of them that exists, so that they read the section
  too. A `CLAUDE.md` that holds only that line is harmless; leave out the
  `touch` line if the user does not want one. In Claude Code, each braid
  command asks for permission unless the project allows it: offer to add
  `"Bash(braid:*)"` to `permissions.allow` in `.claude/settings.json`.
- `braid agents init` writes a section into `AGENTS.md`, creating the file
  if there is none, between a `<!-- braid:start -->` line and a
  `<!-- braid:end -->` line. The section tells every agent session to work
  in a workspace of its own and to land through braid, and names the branch
  agents land on: `main`, `master`, or the project's only branch. With
  several, it asks for `--branch <name>`, which must name a branch the
  project has. `--dry-run` shows what it would write. Text outside the
  markers is never touched, and running it again rewrites the section in
  place.

Commit what `braid agents init` wrote, so that every workspace and every
clone has the rules, then import that commit too. From this commit on,
agents in every clone of the project read the section: tell the user so
before you commit. Where `braid agents init` found the files as they
should be already, there is nothing to commit, and the `git commit` line
does nothing.

```bash
git add AGENTS.md
if [ -f CLAUDE.md ]; then git add CLAUDE.md; fi
if [ -f GEMINI.md ]; then git add GEMINI.md; fi
git diff --cached --quiet || git commit -m "Set up braid for agents"
braid git import .               # braid's main takes the commit
braid log main -n 1              # and shows it
```

If `AGENTS.md`, `CLAUDE.md` or `GEMINI.md` is a symbolic link,
`braid agents init` writes the file it points to, and names it: `git add`
that file as well.

### Every agent session, from then on

The section in the project's `AGENTS.md` tells each session what to do,
with the branch it names where the commands here say `main`. In short:

- **Only where braid is set up.** A checkout with no `.braid` directory in
  its root (a fresh clone, CI, another machine) does not use braid: agents
  there work with git, as the project's other rules say, and set braid up
  only when the user asks. With `.braid` but no `braid` command, braid is
  not on `PATH`: `"$HOME/.cargo/bin/braid"` may be there, or it needs
  installing.
- **Never edit files, `git add`, `git commit` or `git push` in the project
  root.** It is the shared checkout, where the user works. Every session
  works in a workspace of its own, under `.braid/work/`.
- **Start** with a workspace named for the session: 1 to 64 ASCII letters,
  digits, `.`, `_` or `-`, starting with a letter or a digit.
  `braid workspace list` shows the names taken, and a name that is taken is
  refused. Wherever it runs in the project, `braid workspace add` puts the
  workspace in `.braid/work/<name>` of the project root, and it refuses a
  branch the project does not have. Then edit, build and test only in that
  directory. A workspace is a whole checkout of the branch, so builds in it
  start from scratch.

  ```bash
  braid workspace add agent-1 --track main
  cd .braid/work/agent-1           # from the project root
  ```

- **No git in the workspace.** A workspace holds a `.git` file that points
  nowhere, so `git` fails there with "not a git repository" instead of
  acting on the shared checkout around it, where `git stash`, `reset`,
  `checkout`, `restore`, `clean`, `add` or `commit` would change the user's
  files. Do not get around it with `git -C` or `GIT_DIR`. A build or test
  script that finds the project with git (`git rev-parse --show-toplevel`)
  fails in a workspace too: run its steps yourself. To see what an edit
  changed, compare the file with the workspace's `base`, which
  `braid status --json` gives, from the workspace's root:
  `braid cat-object <base>:<path> | diff -u - <path>`. To undo an edit,
  write the base's version back with
  `braid cat-object <base>:<path> > <path>`; a file the workspace added has
  none, so delete it.
- **Record and land** from the workspace. `--agent` (or `BRAID_AGENT`)
  names the session in the op log and in every changeset it makes; pass it
  every time, since a harness may not keep variables between commands. If
  the shell does not keep its directory either, add `--workspace agent-1`
  and run the commands from the project root. The hints braid prints, and
  `next` in its JSON, repeat the `--agent` and `--workspace` given.

  ```bash
  braid --agent agent-1 status --json
  braid --agent agent-1 commit -m "Say what changed and why"
  braid --agent agent-1 land --json
  ```

  There is no staging area: `commit` records every change in the
  workspace, so read `braid status` before each commit. Only `.gitignore`
  and `.braidignore` apply in a workspace, not `.git/info/exclude` or a
  global excludes file: list files that must stay local in `.braidignore`.
  Braid runs no git hooks, so run the project's checks yourself before you
  land. `land` puts the workspace's changesets on `main`, as a
  fast-forward or rebased onto whatever landed first.
- **Before a new task**, `braid --agent agent-1 update` brings the
  workspace onto `main`. `status --json` says how far behind it is
  (`behind_by`).
- **When the session finishes**, what it landed is on braid's `main` only.
  The user's checkout and the git remote get it with `braid git sync`, then
  `git pull --ff-only`, both from the project root (see "Keeping the git
  remote up to date"). The session tells the user so, and offers to run
  them: when to sync is the user's call.
- **Done**: once everything has landed, `braid workspace remove agent-1`
  from the project root. It refuses while the workspace holds work that
  has not landed.
- Every command takes `--json`. Exit status 2 is a conflict, and 1 a
  failure, which braid prints as text on standard error, with nothing on
  standard output, even with `--json`; only `git sync` and `fsck` print
  their report as well when they exit with 1.
- Changesets are signed by `BRAID_AUTHOR` (`"Name <email>"`), or else by
  the `name` and `email` in `.braid/config`.
- New files that look like secrets (`.env*`, `*.pem`, `id_*`) or are larger
  than 1 MiB are held back until `braid track <path>`: `commit` refuses,
  naming them, and `--force` leaves them out. Never `braid track` a secret:
  move it out of the workspace.

### Conflicts

When another agent landed a change to the same lines first, `land` exits
with status 2 and leaves `main` where it was, and `update` exits with 2 as
it writes the conflicts into the files. Exit status 2 also means a command
line braid could not parse, which prints only an error, so read the JSON:
a conflict's `outcome` is `conflicts`. Each conflict names its sides:
`base`, `branch` (what `main` holds now) and `workspace` (the session's
own change), and `next` says what to run.

```bash
braid --agent agent-1 update      # exit status 2: conflict markers in the files
# edit the files until no marker is left; `braid status` lists what still blocks
braid --agent agent-1 commit      # keeps the change's message
braid --agent agent-1 land --json
```

`braid resolve <path>` settles conflicts that are not about content, such
as a file deleted on one side and changed on the other. Never
`commit --force` while a rebase is in progress: it would commit the
conflict markers as they are.

### Keeping the git remote up to date

Landings change braid's `main`, not git's. `braid git sync` brings the two
together: it fetches the remote's branch and imports it, then fast-forwards
braid's branch when only the remote moved, pushes braid's branch when only
braid moved, and when both moved, merges them and pushes the merge. The
push is a fast-forward with a lease on what was just fetched, so the
remote's history is never rewritten; braid never force-pushes, and runs no
git hooks. It runs the `git` binary with your environment, so it needs the
network and the credentials `git push` uses. Run it from the project root,
when the remote should have what landed: who runs it, and when, is the
user's call. Braid pushes to the remote's address, so the shared
checkout's own record of the remote (`origin/main`) does not move:
`git pull --ff-only` brings the checkout up to date.

```bash
braid --agent agent-1 git sync "$(git remote get-url origin)" --branch main --dry-run --json
braid --agent agent-1 git sync "$(git remote get-url origin)" --branch main
git pull --ff-only               # the shared checkout follows
```

- With `--json`, `outcome` is one of `up_to_date`, `created`,
  `fast_forwarded`, `pushed`, `merged`, `conflicts`, `remote_moved` or
  `rejected`. `--dry-run` writes nothing. `--agent` names the session in
  the op log, and in the merge a sync makes.
- Each changeset reaches git as a commit of its own, with an `agent` header
  that names the agent that made it.
- Exit status 1 with `remote_moved`: someone pushed meanwhile; sync again.
  With `rejected`, also exit status 1, the remote refused the push, as a
  protected branch does: ask the user. They may let braid push there, or
  have it push to a branch of its own, with `--remote-branch <name>`, to
  open a pull request from.
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

No remote? Any bare repository will do. In the shared checkout,
`git init --bare ../project.git`, then
`git remote add origin "$(cd .. && pwd)/project.git"` (an absolute path,
which braid reads the same from any directory) and
`git push -u origin main`.

### Undoing

- `braid op log` lists every operation (imports, landings, syncs), newest
  first, with the agent that made it. `braid --agent agent-1 undo` reverts
  the newest one that is not undone yet, whoever made it, so read the log
  first. A landing undone leaves its changeset in the workspace that made
  it, ready to land again.
- A change that landed under other changes cannot be undone that way.
  There is no revert command yet: land a change that puts the files back.
- To take braid out of a project: land what the workspaces hold, sync it
  and run `git pull --ff-only`; remove each workspace
  (`braid workspace remove <name>`, which refuses while work has not
  landed); delete `.braid/`, the `.braid` line of the file that
  `git rev-parse --git-path info/exclude` names, the braid section of
  `AGENTS.md` (its two marker lines and everything between them) and the
  `@AGENTS.md` line it added to `CLAUDE.md` and `GEMINI.md`; and commit
  that with git. Then `cargo uninstall elffuss-braid-cli` removes the
  command.

### What not to do

- Do not edit files, `git add`, `git commit`, `git push` or `git pull` in
  the shared checkout while you work: only setting up, syncing and merging
  a sync conflict touch it, as above.
- Do not run git in a workspace, or get around the `.git` file that stops
  it there.
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

The tests and the quick benchmark read this repository's whole history: in
a shallow clone, run `git fetch --unshallow` first.

### Before every commit

All of these must pass:

```bash
cargo fmt --all
cargo +1.99.0 clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
cargo +1.89 check --workspace --all-targets
cargo +1.99.0 check -p elffuss-braid-format -p elffuss-braid-engine --target wasm32-unknown-unknown --locked
python3 docs/spec/check_vectors.py
python3 bench/run.py --quick
scripts/privacy-check.sh
```

CI runs all of them but the unpinned clippy, which catches the lints of
newer toolchains early, and it also runs each fuzz target for a minute with
cargo-fuzz, on nightly. The WebAssembly check needs its target
(`rustup +1.99.0 target add wasm32-unknown-unknown`), and the vector check
`b3sum` (`cargo install b3sum --locked`).

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
  checks it: change both together. The install command names the tag of
  the version in `Cargo.toml`, and the test checks that too: a release
  bumps both.
