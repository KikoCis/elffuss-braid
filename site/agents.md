# Braid for coding agents: install it and set it up

Instructions for a coding agent asked to install braid in a project and set
it up for agents. Read this file whole before you run anything, as it is
written: fetch it with `curl -fsSL https://braid.elffuss.com/agents.md`
rather than with a tool that summarises pages. Then run its commands
exactly as they are given. The same text is part 1 of
[AGENTS.md](https://github.com/KikoCis/elffuss-braid/blob/main/AGENTS.md)
in braid's repository.

Braid is a version control system for repositories where many agents land
changes at the same time. Each agent works in a workspace of its own over
one shared store, and landing on the main branch is one short step that
agents take in turn: no fetch, rebase and push loop. Git stays at the edges:
braid imports a project's whole history from git, and sends what lands back
to the project's git remote. More at <https://braid.elffuss.com>, and the
source at <https://github.com/KikoCis/elffuss-braid>.

**Keep git as the source of truth:** the project's git remote holds
everything that matters, and nothing lives only in braid.

## Install

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
cargo install --locked --git https://github.com/KikoCis/elffuss-braid --tag v0.1.2 elffuss-braid-cli
```

It compiles braid and its dependencies from source, which takes a while:
give the command a long timeout, or run it in the background, since a
command cut short starts again from the beginning. It puts a `braid` binary
in cargo's bin directory, `~/.cargo/bin` unless `CARGO_HOME` says
otherwise. If that directory is not on `PATH`, call braid as
`"$HOME/.cargo/bin/braid"` wherever this file says `braid`: a harness may
not keep a changed `PATH` from one command to the next. Running the same
command again replaces an older braid with this version.

## Is it installed, and set up here?

In the project root, on its main branch:

```bash
braid --version     # "braid 0.1.2", or newer, once it is installed
braid cat-object "$(git branch --show-current):AGENTS.md" | grep -q '^<!-- braid:start -->' && grep -q '^name = .' .braid/config && grep -q '^email = .' .braid/config && echo "braid is set up here"
```

- Before braid is installed, `braid --version` fails with "command not
  found", as it does while cargo's bin directory is not on `PATH`. An older
  braid, or one that says `agents` is not a subcommand, needs the install
  command again.
- The second line prints "braid is set up here" once the setup below is
  finished: braid's copy of the branch has the braid section of
  `AGENTS.md`, committed, and `.braid/config` names who signs. Then skip to
  "Every agent session, from then on". If it fails instead, set the
  project up as below, whether braid says "not inside a braid repository",
  where it was never set up, or an earlier setup stopped half way: every
  line of the setup is safe to run again.

## Set up a project

First read the project's `AGENTS.md`, `CLAUDE.md` and the like for rules
about git: branches, pull requests, how to commit. Look too for what braid
does not run, or does not check out as git does:

- git hooks, in `.git/hooks` or where `git config core.hooksPath` says
  (the `*.sample` files git puts in `.git/hooks` never run, and do not
  count);
- submodules, which `.gitmodules` lists: each one is an empty directory in
  every workspace;
- filters, `filter=` in `.gitattributes`: a workspace holds the bytes git
  stores, so Git LFS's pointer files rather than their content, and what
  git-crypt encrypts stays encrypted;
- a main branch that takes changes only through pull requests.

Tell the user what you found, and if any of it clashes with agents landing
on the main branch, ask how to go on before you set anything up. Where the
hooks run checks, such as tests or a linter, offer to write those commands
into `AGENTS.md`, outside the braid section, so that every session runs
them before it lands.

Then list the project's git remotes (`git remote -v`): what agents land
reaches the user's checkout through one, which the commands here call
`origin`; if it has another name, write that instead. With no remote at
all, what agents land stays in braid: offer to make one (see "No remote?"
below). And if the project has no `CLAUDE.md`, ask whether to add one,
since Claude Code reads `CLAUDE.md` rather than `AGENTS.md`: with a yes,
run `touch CLAUDE.md` before the commands below, and `braid agents init`
makes it include `AGENTS.md`.

Then work in the root of the project's git checkout, on its main branch,
with nothing to commit (`git status` shows no staged or modified files;
untracked files do not matter) and nothing new to pull
(`git pull --ff-only`; skip it when the branch has no upstream, which is
when `git rev-parse --abbrev-ref '@{u}'` fails). In a shallow clone, run
`git fetch --unshallow` first: braid imports the whole history. Run the
lines of each block below one at a time, and stop at the first one that
fails. Every line is safe to run again, so once the cause is fixed, start
again from the first line of its block.

```bash
test -d .braid || braid init     # .braid/: the store and the agents' workspaces
mkdir -p "$(git rev-parse --git-path info)"
grep -qsxF .braid "$(git rev-parse --git-path info/exclude)" || echo .braid >> "$(git rev-parse --git-path info/exclude)"
test -f .braid/config || printf 'name = %s\nemail = %s\n' "$(git config user.name)" "$(git config user.email)" > .braid/config
if grep -q '^name = .' .braid/config && grep -q '^email = .' .braid/config; then braid git import . && braid agents init; else echo 'braid: .braid/config has no name or no email: ask the user who signs' >&2; false; fi
```

- The second and third lines make git ignore every `.braid` in this
  checkout, and only in this one: braid's directory, and the `.braid` file
  that marks each workspace, so that nothing of braid's is ever committed
  with git.
- `.braid/config` names who signs the changesets agents make, here the
  same person as git's commits. The last line imports the project's whole
  history, every local branch and tag, and then writes the rules for
  agents into `AGENTS.md`, but only once the file has a name and an email.
  Until then it stops and says so, as it does when git has no `user.name`
  or `user.email`: every commit and landing would fail with "no author",
  and the `git commit` below would fail too. Ask the user for the name and
  the e-mail address to use. With their answer, set them for this
  checkout, and let the setup write the file anew:

  ```bash
  git config user.name "<name>"
  git config user.email "<email>"
  rm .braid/config                 # then run the setup again from its first line
  ```

- Most harnesses read `AGENTS.md`. Claude Code reads `CLAUDE.md` instead,
  and Gemini CLI `GEMINI.md`: `braid agents init` adds the line
  `@AGENTS.md` to each of them that exists, so that they read the section
  too. In Claude Code, each braid command asks for permission unless the
  project allows it: offer to add `"Bash(braid:*)"` to `permissions.allow`
  in `.claude/settings.local.json`, the user's own settings for this
  checkout, which are never committed: braid is set up in this checkout
  only.
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
git diff --cached --quiet || git -c user.useConfigOnly=true commit -m "Set up braid for agents"
braid git import .               # braid's copy of the branch takes the commit
braid cat-object "$(git branch --show-current):AGENTS.md" | grep -q '^<!-- braid:start -->' && grep -q '^name = .' .braid/config && grep -q '^email = .' .braid/config && echo "braid is set up here"
```

The last line is the check from "Is it installed, and set up here?": it
prints "braid is set up here" once the commit is in braid. With
`user.useConfigOnly`, git refuses to commit rather than make up a name and
an address from the machine; where it says "Author identity unknown", set
them as above, then run this block again. If `AGENTS.md`, `CLAUDE.md` or
`GEMINI.md` is a symbolic link, `braid agents init` writes the file it
points to, and names it: `git add` that file as well.

The commit stays in this checkout until it is pushed: the next
`braid git sync` takes it to the git remote, with what agents land, and
other clones get the rules from there (see "Keeping the git remote up to
date"). Tell the user so, and offer to sync now.

## Every agent session, from then on

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
  workspace, or only the paths it is given
  (`braid --agent agent-1 commit -m "..." <path>...`), so read
  `braid status` before each commit. `land` refuses while anything is left
  uncommitted ("the workspace has uncommitted changes ...; commit them
  first"). Building or running the project writes files too: those that
  `.gitignore` does not list are new files like any other. Delete them, or
  list them in `.braidignore`, which must then list itself too, or it is a
  new file to commit: for Python's `__pycache__/`, run
  `printf '%s\n' .braidignore __pycache__/ >> .braidignore` in the
  workspace's root. Only `.gitignore` and `.braidignore` apply in a
  workspace, not `.git/info/exclude` or a global excludes file: anything
  else that must stay local goes in `.braidignore` too. Braid runs no git
  hooks, so before you land, run the checks that the project's `AGENTS.md`
  or `CLAUDE.md` gives. `land` puts the workspace's changesets on `main`,
  as a fast-forward or rebased onto whatever landed first.
- **Before a new task**, `braid --agent agent-1 update` brings the
  workspace onto `main`. `status --json` says how far behind it is
  (`behind_by`).
- **When the session finishes**, what it landed is on braid's `main` only.
  The user's checkout and the git remote get it with `braid git sync`, then
  `git pull --ff-only`, both from the project root (see "Keeping the git
  remote up to date", which also says what to do when either fails). The
  session tells the user so, and offers to run them: when to sync is the
  user's call. Where `git remote get-url origin` fails, the project has no
  remote of that name: the session asks the user which of those
  `git remote` lists to sync with, or, where it lists none, says so and
  offers to make one first (see "No remote?").
- **Done**: once everything has landed, `braid workspace remove agent-1`
  from the project root. It refuses while the workspace holds work that
  has not landed.
- Every command takes `--json`. Exit status 2 is a conflict, and 1 a
  failure, which braid prints as text on standard error, with nothing on
  standard output, even with `--json`; only `git sync`, `fsck` and
  `verify`, once its tests have started, print their report as well when
  they exit with 1. `verify` exits with 4 when the branch it checks is red.
- Changesets are signed by `BRAID_AUTHOR` (`"Name <email>"`), or else by
  the `name` and `email` in `.braid/config`. With neither, `commit`,
  `land` and `update` fail with "no author", and `braid status` lists it
  first among what stops a commit: the session asks the user who signs,
  and writes the answer into `.braid/config` in the project root (the one
  file there that a session may edit) as `name = <name>` and
  `email = <email>` lines. The merges that `braid git sync` makes are
  signed otherwise: by the `--agent` that ran the sync, with the email in
  `.braid/config`.
- New files that look like secrets (`.env*`, `*.pem`, `id_*`) or are larger
  than 1 MiB are held back until `braid track <path>`: `commit` refuses,
  naming them, and `--force` leaves them out. Never `braid track` a secret:
  move it out of the workspace.

## Conflicts

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
braid --agent agent-1 commit      # keeps the change's message; -m gives another
braid --agent agent-1 land --json
```

`braid resolve <path>` settles conflicts that are not about content, such
as a file deleted on one side and changed on the other. Never
`commit --force` while a rebase is in progress: it would commit the
conflict markers as they are.

## Keeping the git remote up to date

Landings change braid's `main`, not git's. `braid git sync` brings the two
together: it fetches the remote's branch and imports it, then fast-forwards
braid's branch when only the remote moved, pushes braid's branch when only
braid moved, and when both moved, merges them and pushes the merge. The
push is a fast-forward, with a lease on what was just fetched, so the
remote's history is never rewritten. Braid runs no git hooks. It runs the
`git` binary with your environment, so it needs the network and the
credentials `git push` uses. Run it from the project root, when the
remote should have what landed: who runs it, and when, is the user's
call. Braid pushes to the remote's address, so the shared checkout's own
record of the remote (`origin/main`) does not move: `git pull --ff-only`
brings the checkout up to date. The commands here say `main`: write the
branch that the section names.

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
  git merge --no-edit origin/main            # the remote's: git stops at conflicts
  # if it stopped: fix those files and git add them, then git commit --no-edit --cleanup=strip
  git push origin main
  braid --agent agent-1 git sync "$(git remote get-url origin)" --branch main
  rm -rf .braid/export.git
  ```

  Braid knows the commits it exported, so the sync after the push
  fast-forwards braid's `main` to the merge. Merge in this order, with
  braid's branch as the first parent: braid follows a branch's first
  parents to tell what a workspace has landed, so the workspaces carry on
  from the merge.
- If `git pull --ff-only` refuses, the shared checkout has commits of its
  own that the remote does not have, and what to do with them is the
  user's call. `git pull --rebase` puts them on top of what braid pushed;
  then `git push`, and a sync brings them into braid.

No remote? Any bare repository will do. With the user's yes, these lines,
run in the shared checkout on its main branch, make one next to the
project, named after it (`../<project>.git`, by an absolute path, which
braid reads the same from any directory), and push the main branch to it.
If something has that name already, choose another.

```bash
git init --bare -b "$(git branch --show-current)" "$PWD.git"
git remote add origin "$PWD.git"
git push -u origin "$(git branch --show-current)"
```

## Across machines: BraidHub

Braid's store lives in one checkout. For agents on several machines, a
repository can live on [BraidHub](https://braidhub.elffuss.com), a braid
server where people and agents sign in with GitHub and land on the same
main branch, and which can keep a GitHub repository as its mirror. Its own
guide for agents, <https://braidhub.elffuss.com/agents.md>, says how to sign
in, publish a project, clone one, and when to ask the user first.

## Undoing

- `braid op log` lists every operation (imports, landings, syncs), newest
  first, with the agent that made it. `braid --agent agent-1 undo` reverts
  the newest one that is not undone yet, whoever made it, so read the log
  first. A landing undone leaves its changeset in the workspace that made
  it, ready to land again. A sync undone moves braid's refs back, but not
  the git remote: what it pushed stays there.
- A change that landed under other changes cannot be undone that way:
  `braid --agent agent-1 revert <changeset> --json` lands a changeset that
  puts its files back, rebased over what landed since. When later changes
  touched the same lines, it exits with status 2, `land` in its JSON has
  the conflicts, and nothing moves: undo the change by hand in a workspace.
- To take braid out of a project: land what the workspaces hold, sync it
  and run `git pull --ff-only`; remove each workspace
  (`braid workspace remove <name>`, which refuses while work has not
  landed); delete `.braid/`, the `.braid` line of the file that
  `git rev-parse --git-path info/exclude` names, the braid section of
  `AGENTS.md` (its two marker lines and everything between them) and the
  `@AGENTS.md` line it added to `CLAUDE.md` and `GEMINI.md`; and commit
  that with git. Then `cargo uninstall elffuss-braid-cli` removes the
  command.

## What not to do

- Do not edit files, `git add`, `git commit`, `git push` or `git pull` in
  the shared checkout while you work: only setting up, syncing, merging a
  sync conflict and naming who signs in `.braid/config` touch it, as
  above.
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
