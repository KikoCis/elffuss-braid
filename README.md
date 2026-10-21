<picture>
  <source media="(max-width: 1011px) and (prefers-color-scheme: dark)" srcset="docs/assets/hero-phone-dark.svg">
  <source media="(max-width: 1011px)" srcset="docs/assets/hero-phone-light.svg">
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/hero-dark.svg">
  <img src="docs/assets/hero-light.svg" width="100%" alt="Elffuss Braid: many agents, one main, no retry loop. Under it, a terminal shows three agents landing on main in turn: agent-a lands as a fast-forward, agent-b is rebased onto it, and agent-c, which changed the same line of shipping.toml as agent-a, gets a conflict, so main did not move and braid exits with status 2.">
</picture>

# ✳ Elffuss Braid

**Many agents. One main. No retry loop.** Version control for repositories
where many AI coding agents land changes at the same time, written from
scratch in Rust. It installs with one `cargo install`, needs no server on
one machine, and keeps git at the edges, so GitHub keeps working.

[![CI](https://img.shields.io/github/actions/workflow/status/KikoCis/elffuss-braid/ci.yml?branch=main&style=flat-square&label=CI&logo=githubactions&logoColor=white)](https://github.com/KikoCis/elffuss-braid/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-c8a06a?style=flat-square&logo=apache&logoColor=white)](LICENSE)
[![Version 0.1.1](https://img.shields.io/badge/version-0.1.1-54dcc6?style=flat-square)](#status-and-roadmap)
[![Rust 1.89+](https://img.shields.io/badge/rust-1.89%2B-8272ff?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Format v1: frozen](https://img.shields.io/badge/format%20v1-frozen-c8a06a?style=flat-square)](docs/spec/format-v1.md)

[Quick start](#quick-start) · [Ask your agent](#use-it-from-your-agent) · [Scion's ideas](#scions-ideas-in-braid) · [How it works](#how-it-works) · [BraidHub](#braidhub) · [Measured against git](#measured-against-git) · [braid.elffuss.com](https://braid.elffuss.com) · [En castellano](#en-castellano)

Anthropic described Scion, the version control system it built in-house
with agents in mind, in
[Redesigning source control at Anthropic](https://www.anthropic.com/engineering/redesigning-source-control).
Scion is internal. Braid is an independent, open-source take on several of
its ideas, and you can run it today:
[ask your coding agent](#use-it-from-your-agent) to set it up in a
project, or try the [quick start](#quick-start).

*Braid contains no code from Anthropic, and is not affiliated with or
endorsed by Anthropic.*

> [!NOTE]
> Version 0.1: keep your git repository as the source of truth.

## Scion's ideas, in braid

Each row is an idea from the article, what braid 0.1.1 does with it, and
how far along that is.

| Scion's idea | In braid 0.1.1 | State |
|---|---|---|
| A changeset lists the files changed against its first parent | The same, plus the root tree id; the bytes are [frozen](docs/spec/format-v1.md) | ✅ done |
| Data is content-addressed and never changes, so any cache can serve it | BLAKE3 ids; BraidHub serves every object by id as immutable | ✅ done |
| Commits take no lock, only moving a branch is serial, and the server does the rebases | Agents commit in parallel; `braid land` rebases path by path onto whatever landed first, in one short step taken in turn, locally or on BraidHub | ✅ done |
| A commit costs what it changes, not what the repository weighs | Landing rebases only the paths a change touches, and `braid change create` needs no working copy; but `braid commit` scans the whole working copy, and at the last measurement was slower than git on large trees ([measured](#measured-against-git)) | partly |
| Derived data for blame, path history and ancestry, computed from the parents | Generation numbers, skip pointers, changed-path filters and blame records, computed once and kept (`braid derive`); BraidHub serves history and diffs, not blame | ✅ done locally |
| Every query is an API, and the working copy is optional | `--json` on every command; `braid hub show`, `ls`, `cat`, `log` and `diff` read a repository with no clone; `braid change create --land` writes with no checkout | first version |
| Sparse and virtualized checkouts | `braid hub clone --paths` brings only the trees under some paths; a virtual file system is planned (milestone 7) | first version |
| The daemon, the caches and the server answer the same requests | BraidHub is the server; a per-machine daemon and cache tiers are planned (milestones 6 and 7) | planned |
| No merge queue: test after merging, bisect, revert | [`braid verify --revert`](#keeping-main-green) bisects to the changeset that broke the tests and lands its revert | ✅ new in 0.1.1 |
| Concurrent changes can conflict, and agents fix them | A conflict comes back as data, with the command to run `next`, and `main` does not move; braid has no resolver of its own | ✅ done |
| Hermetic simulations and fuzzing | Deterministic simulation of agents landing at once, with injected faults; crash and power-loss tests; five fuzz targets | ✅ done |
| Permissions per directory, semantic approval rules, monitoring agents ("superfans") | BraidHub's permissions are per repository, and its change requests take reviews and approvals | not in braid |
| Clients are always online | Braid works offline on one machine; BraidHub is the authority when several machines share a repository | no, by design |

## Three agents, one main

Three agents change a small shop at once, each in a workspace of its own:
no index to fight over, no lock. Then they land on `main`, in turn.

![Three agents commit at the same time, each in its own workspace, and land on main in turn: the first is a fast-forward, the second is rebased onto it, and the third changed the same line as the first, so main does not move and braid reports the conflict with exit status 2](docs/assets/demo-agents.gif)

*The real CLI, recorded by [`scripts/demos/render.sh`](scripts/demos/render.sh).*

- **agent-a** lands first: a fast-forward.
- **agent-b** is one changeset behind. It does not fetch and does not
  retry: `braid land` rebases its change onto agent-a's, in one short step.
- **agent-c** changed the same line as agent-a, a real conflict: `main`
  stays where it was, braid exits with status 2, and the conflict comes
  back as data, with each side and the command to run `next`.

In git, each agent that loses the race fetches, rebases and pushes again.
In the run [measured against git](#measured-against-git), on one machine,
with 4, 8 and 16 agents landing at once, git retried each push 1.1, 2.5 and
5.2 times on average, no braid agent retried, and braid took 0.4, 0.3 and
0.2 of git's time. CI runs a scene like this one on every push to `main`
([`three_agents.rs`](crates/cli/tests/three_agents.rs)).

## Quick start

Braid builds from source with cargo, and needs Rust 1.89 or newer, a C
compiler and git. CI tests Linux and other Unix-like systems; Windows is
not tested yet.

```bash
cargo install --locked --git https://github.com/KikoCis/elffuss-braid --tag v0.1.1 elffuss-braid-cli
```

To set braid up in a project you already have,
[ask your agent](#use-it-from-your-agent). To run the race above yourself,
in a throwaway directory:

```bash
export BRAID_AUTHOR="Ada Lovelace <ada@example.org>"
cd "$(mktemp -d)" && braid init shop && cd shop
echo 'free_over = 60' | braid change create --base none \
  --set shipping.toml=@- -m "Start the shop" --land main > /dev/null
for a in a b c; do braid workspace add agent-$a; done      # under .braid/work/

echo 'free_over = 50'  > .braid/work/agent-a/shipping.toml
echo 'Ships in a day.' > .braid/work/agent-b/README.md
echo 'free_over = 40'  > .braid/work/agent-c/shipping.toml
braid commit --workspace agent-a --agent agent-a -m "Free shipping from 50"
braid commit --workspace agent-b --agent agent-b -m "Next-day shipping"
braid commit --workspace agent-c --agent agent-c -m "Free shipping from 40"

braid land --workspace agent-a --agent agent-a     # fast-forward
braid land --workspace agent-b --agent agent-b     # rebased onto agent-a's change
braid land --workspace agent-c --agent agent-c     # a conflict: exit status 2
```

The three landings print this; your ids will differ, since every signature
carries its time:

```console
Landed 9b6231894f8d on main (fast-forward)
Landed on main as 76b20fe96330, rebased onto 9b6231894f8d
Updated 1 path in the working copy
bdc82892d520 conflicts with main at 76b20fe96330; main did not move:
  CONTENT       shipping.toml  (1 region)
Run `braid --agent agent-c --workspace agent-c update` to bring the conflicts into the working copy.
```

<details>
<summary>The same conflict as an agent reads it, with --json</summary>

`braid land --json`, abridged:

```json
{
  "outcome": "conflicts",
  "next": "braid --agent agent-c --workspace agent-c update",
  "conflicts": [{
    "path": "shipping.toml",
    "kind": "CONTENT",
    "hunks": [{
      "base":      { "start": 0, "end": 1, "text": "free_over = 60\n" },
      "branch":    { "start": 0, "end": 1, "text": "free_over = 50\n" },
      "workspace": { "start": 0, "end": 1, "text": "free_over = 40\n" }
    }]
  }]
}
```

`base` is what both sides started from, `branch` what `main` holds now and
`workspace` the agent's own change; lines count from 0, and `end` is left
out. Exit status 2 also means a command line that braid could not parse, so
an agent reads `outcome` too. `braid land --help` describes every field.

</details>

Settle the conflict as an agent would, then look back:

```bash
braid update --workspace agent-c --agent agent-c    # exit status 2: markers in its shipping.toml
echo 'free_over = 45' > .braid/work/agent-c/shipping.toml
braid commit --workspace agent-c --agent agent-c -m "Free shipping from 45"
braid land --workspace agent-c --agent agent-c      # fast-forward
braid log main     # every changeset names the agent that made it
braid op log       # every landing, newest first
braid undo         # takes agent-c's landing back; the change stays in its workspace
```

![The third agent asks for the conflict as JSON and gets the base, branch and workspace text of the line; braid update writes conflict markers into the file; the agent settles the line, commits and lands, and braid log shows the three agents' changesets on main](docs/assets/demo-conflict.gif)

*The recording goes on the same way, and asks for the conflict as JSON first.*

## Use it from your agent

Braid comes with instructions written for coding agents. In a project that
is a git repository, ask your agent, whatever harness runs it:

```text
Install braid in this project and set it up for agents, following https://braid.elffuss.com/agents.md (read it whole, with curl -fsSL)
```

It installs braid with cargo, tells you what in the project braid does not
run, such as git hooks, offers to make a git remote if the project has
none, imports the project's git history, and writes the rules for agents
into the project's `AGENTS.md` with `braid agents init`. From then on,
every agent session works in a braid workspace of its own, where git
refuses to run, lands through braid, and tells you when what it landed is
waiting for a sync to the git remote. In a clone where braid is not set up,
agents work with git as before.

![Setting braid up in a project's git checkout: braid init, git told to ignore .braid, braid git import brings in the history, braid agents init writes the braid section into AGENTS.md, the section's rules are listed, and a new session starts in a workspace of its own under .braid/work](docs/assets/demo-agents-init.gif)

- **`AGENTS.md`** gets a section between `<!-- braid:start -->` and
  `<!-- braid:end -->` with the rules every session follows; running
  `braid agents init` again rewrites only that section. **`CLAUDE.md`** and
  **`GEMINI.md`**, where they exist, get the line `@AGENTS.md`. They are
  committed with git, so every clone and every new session has them.
- **Nothing of braid's own goes into git.** `.braid/` holds the store, the
  workspaces and `.braid/config`, the name and e-mail address that sign
  what agents land.

Agents get `--json` on every command, exit status 2 for a conflict that
left the branch where it was, and a guard that holds back new files that
look like secrets (`.env*`, `*.pem`, `id_*`) or are larger than 1 MiB until
`braid track` lets them in. The guide is part 1 of [AGENTS.md](AGENTS.md),
served as [braid.elffuss.com/agents.md](https://braid.elffuss.com/agents.md)
next to an [llms.txt](https://braid.elffuss.com/llms.txt). To set a project
up by hand, follow its [Set up a project](AGENTS.md#set-up-a-project): the
same steps, what to check first, and what to do when one fails.

## Why braid

Git was designed for people who work offline and sync now and then. Agents
are many, always connected, and all want to land on `main` at once. With
git, every agent that loses the race to push fetches, rebases and pushes
again. A merge queue ends the race by testing each change before it lands,
so every landing waits for CI; Anthropic's article puts it plainly: "A
proper merge queue, for example, doesn't make much sense at agent scale."

| | git, a clone per agent | a merge queue | braid |
|---|---|---|---|
| Landing | `git push`, rejected whenever another agent got in first | waits for CI on the changes ahead of it | `braid land`: one short step, taken in turn |
| Retries | grow with the agents: 5.2 per push with 16, [in one run](#measured-against-git) | none | none: a change that fell behind is rebased |
| A conflict | markers, and messages written for people | the change leaves the queue | exit status 2, `main` does not move, JSON with `next` |
| A change that breaks the build | lands | is stopped before it lands | lands, and [`braid verify`](#keeping-main-green) then lands its revert |

So braid trades the queue's guarantee for landings that never wait for CI:
between a landing and the next `braid verify`, `main` can be red, and
`braid/green/main` names the newest changeset that passed.

<details>
<summary>The design, against git's</summary>

| | git | braid |
|---|---|---|
| Unit of change | a commit: a full tree snapshot | a changeset: the files that changed against the first parent, plus the root tree id |
| Many writers | one index per working tree | one workspace per agent over a shared append-only store; creating a change takes no repository lock |
| Fetching | negotiated packs, built for each client | immutable objects by id, which any HTTP cache can serve |
| Large directories | rewritten whole on every change | split into content-defined nodes; an edit rewrites only its path |
| Large files | a separate extension | content-defined chunks, deduplicated across versions |
| Hash | SHA-1 by default | BLAKE3 |
| For agents | output written for people | `--json` everywhere; changes can be created without a checkout |

This is the design. [Status and roadmap](#status-and-roadmap) says what
exists today.

</details>

## How it works

<picture>
  <source media="(max-width: 1011px) and (prefers-color-scheme: dark)" srcset="docs/assets/how-it-works-tall-dark.svg">
  <source media="(max-width: 1011px)" srcset="docs/assets/how-it-works-tall-light.svg">
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/how-it-works-dark.svg">
  <img src="docs/assets/how-it-works-light.svg" alt="Three workspaces, a, b and c, each with its own working copy and no index, sit on one shared store of immutable objects named by BLAKE3 and an append-only op log, and commit in parallel. Landing is one short step, taken in turn: a goes straight in as a fast-forward, b is rebased onto it, and c hits a conflict, exit 2, so main did not move; c runs update, fixes the file and commits, then lands as a fast-forward. The strands braid into main, which git sync sends to any git remote as a fast-forward, with a lease. A dashed box marks a daemon and caches as planned.">
</picture>

1. **Everything is an immutable object named by its BLAKE3 hash,** and a
   changeset records the files that changed against its first parent, plus
   the root tree it produces: history, blame and rebase read the short
   list, and the root id commits to the whole repository.
2. **Landing is the one step agents take in turn, and it is short:** a
   fast-forward, or a rebase path by path that merges only the files both
   sides touched. Conflicts come back as data, and the branch does not
   move.
3. **Many processes share one store without a daemon,** through locks the
   kernel releases if a process dies, and an append-only op log records
   every operation, so that `braid undo` can step back.
4. **The same requests at every tier:** a local store, a per-machine
   daemon, a cache and a server are meant to answer the same requests. The
   server has its first version, BraidHub; the daemon and the caches are
   planned (milestones 6 and 7).

## Working with git

Repositories come in with their whole history, and git hosts keep working
as mirrors.

![braid git sync takes a branch of a git repository with its history; after agents land two changes in braid and a person pushes one to the git repository, the next sync merges both and pushes a fast-forward, and git log shows the original commits with their ids unchanged](docs/assets/demo-git-sync.gif)

```bash
cd "$(mktemp -d)" && braid init demo && cd demo
braid git import /path/to/project       # every local branch and tag
braid log main -- README.md             # only the changes to a path
braid blame main README.md              # who last changed each line
git init --quiet --bare ../mirror.git
braid git export ../mirror.git          # the same commits, with the same ids
braid git sync https://github.com/you/project.git --branch main
```

- **Bit for bit.** `braid git import` reads git through
  [gitoxide](https://github.com/GitoxideLabs/gitoxide), exports each commit
  again as it imports it, and stops unless the id comes back the same.
  Shallow and SHA-256 repositories are refused for now.
- **The remote only fast-forwards.** `braid git sync` fetches and imports,
  then fast-forwards braid or pushes, merging first when both moved. The
  push carries a lease on what it just fetched, so history on GitHub, Gitea
  or any git remote is never rewritten; a conflict moves nothing and exits
  with status 2.
- **No git code.** Braid contains no code from git itself, which is GPLv2.
  It reads and writes git data with gitoxide (MIT or Apache-2.0);
  `git sync` also runs the `git` binary, to set up its mirror, fetch and
  push.

## Keeping main green

New in 0.1.1. Braid has no merge queue: changes land as they come, and
`braid verify` checks afterwards. It runs the project's tests on the head
of a branch, in a fresh copy of its files, and keeps `braid/green/<branch>`
at the newest changeset that passed. When they fail, it bisects the
changesets since the green one, and with `--revert` it lands the revert of
the first that fails. One run is one pass: run it after landings, or on a
schedule.

```bash
braid verify --branch main --revert --json -- cargo test --workspace
```

Here three changesets landed after the green one, and the second broke the
tests. A failing run is run once more before it counts:

```console
$ braid verify --branch main --revert -- sh -c 'grep -qx ok status.txt'
braid verify: 60fb322bafea failed (exit status: 1)
braid verify: 60fb322bafea failed (exit status: 1)
braid verify: c4b445c63d60 passed
braid verify: 51108ceb6ded failed (exit status: 1)
braid verify: 51108ceb6ded failed (exit status: 1)
main at 60fb322bafea: reverted (5 runs)
the tests fail from 51108ceb6ded, the first changeset that fails; its revert landed on main
Revert: dbc25836e20d
```

Exit status 0 means green, 4 red (whether or not a revert landed), 2 a
revert that conflicts, and 1 an error. With no green ref yet, as in a fresh
clone, verify tests only the head, unless `--from` names a changeset to
bisect from. The fresh copy is not a sandbox: the command runs as you, with
your files and the network, so run verify in a container or as a user of
its own for code you do not trust. `braid revert <rev>` undoes one
changeset by hand. `braid verify --help` has the rest, and
[the design](docs/design.md#landing-on-a-branch) has the rules.

## BraidHub

[BraidHub](https://braidhub.elffuss.com) is the hosted service for agents
on many machines. In a repository linked to it, `braid land` lands on the
hub, rebased there path by path in turn with every other landing, with the
output and exit statuses of a local landing. A repository is private unless
its owner makes it public, and can keep a GitHub repository as its mirror,
which every landing on its default branch reaches as a fast-forward.

A public repository can be read, and cloned, with no account. This one is
on the hub too:

```bash
braid hub show -R kikocis/elffuss-braid     # its description, counts and README
braid hub log -R kikocis/elffuss-braid      # its history, newest first
braid hub cat -R kikocis/elffuss-braid Cargo.toml
braid hub clone kikocis/elffuss-braid spec --paths docs/spec   # only the trees under docs/spec
```

Landing from several machines takes an account, with GitHub sign-in:
`braid login`, then `braid hub publish` to put a repository on the hub and
`braid hub clone` to make a linked copy of it elsewhere. The same CLI
covers change requests that land exactly as they were reviewed, issues, an
inbox and forks, every command with `--json`. Coding agents have
[their own guide](https://braidhub.elffuss.com/agents.md)
([docs/hub-agents.md](docs/hub-agents.md)), and [docs/hub.md](docs/hub.md)
documents the protocol. BraidHub is new in version 0.1: for anything that
matters, keep a GitHub mirror as the source of truth.

## Design

- [Design overview](docs/design.md): the store, landing, history,
  workspaces, the git bridge, the server, and how it is tested.
- [Format v1](docs/spec/format-v1.md): the exact bytes of ids, trees and
  changesets, with golden vectors.
- [The BraidHub protocol](docs/hub.md) and
  [BraidHub for coding agents](docs/hub-agents.md).
- [Benchmarks against git](bench/README.md): how every ratio here is
  measured.

## Status and roadmap

| Milestone | Scope | State |
|---|---|---|
| 0 | Public repository, CI, object ids, `braid hash-object` | ✅ done |
| 1 | Format v1: canonical codec, trees (flat and split), changesets, name rules; frozen | ✅ done |
| 2 | Local store and op log: many processes, crash-safe, `fsck`, `undo` | ✅ done |
| 3 | History and the git bridge: import, export, `log`, `show` | ✅ done |
| 4 | Many agents on one machine: workspaces, `land`, conflicts, `git sync` | ✅ done |
| 5 | Performance: deltas, chunking, derived indexes, `blame`, benchmarks against git | in progress |
| 6 | Server: [BraidHub](#braidhub) with GitHub sign-in, landing over HTTP, GitHub mirrors and sparse clones; caching tiers | first version; caching planned |
| 7 | Daemon, virtual file system, WebAssembly core | planned |

New in 0.1.1: `braid revert`, and `braid verify`, which tests a branch's
head, bisects a failure and lands the revert of the changeset that caused
it.

Each milestone ships when its tests pass: golden vectors and fuzzing for
the format, crash and many-process tests for the store, bit-for-bit round
trips and differential tests against git for the bridge, deterministic
simulation for landing.

## What is honest about this

- **It is version 0.1.** Today it stores history, round-trips git
  repositories, runs workspaces for many agents on one machine, lands
  their work from many machines through BraidHub, and finds and reverts
  what broke the tests. Keep your git repository as the source of truth,
  and do not keep anything only in braid.
- **Slower than git at most things, at the last measurement.**
  [Measured against git](#measured-against-git) gives every result as a
  ratio against git on the same machine, with the script that reproduces
  it. At commit `5330054`, braid was faster when many agents landed at
  once, and slower at importing and storing history, and at status and
  commit on large trees. Much of that has been worked on since, and has
  not been measured again.
- **`main` can be red for a while.** Braid lands first and tests after,
  and merges are textual: two changes can merge cleanly and still break
  the build together, until [`braid verify`](#keeping-main-green) reverts
  one of them.
- **Not there yet:** garbage collection, so nothing is ever removed from a
  store, and shallow or SHA-256 git repositories, which the git bridge
  refuses. `log -- <path>` and `blame` follow first parents and, on
  purpose, not renames.
- **The hashed format is frozen,** and every golden vector is recomputed by
  a second implementation written from the specification alone. Storage
  and protocols can still change.
- **Braid is not Scion.** It starts from the ideas Anthropic described in
  public about its own source control system. It is an independent
  implementation, contains no code from Anthropic, and is not affiliated
  with or endorsed by Anthropic.
- **Platforms.** CI tests Linux and other Unix-like systems. Windows is not
  tested yet.

## Measured against git

Each number is braid's time, or the size of its store, divided by git's for
the same work on the same machine: below 1 braid is faster or smaller,
above 1 it is slower or larger. They are medians from one run of
[`bench/run.py`](bench/README.md), measuring braid at commit `5330054`. Git
ran with its defaults, except that automatic garbage collection and
maintenance were off, and so were its file system monitor and, unless a row
says otherwise, its untracked cache. [The benchmark's README](bench/README.md)
describes every scenario. Yours will differ: run
`python3 bench/run.py --only <scenario>` before relying on any of them.

| What was compared | braid / git | `--only` |
|---|---|---|
| 4, 8 and 16 agents land 10 commits each, at once: `braid commit` + `braid land`, a workspace each, vs `git commit` + `git push`, a clone each, with `git pull --rebase` and a new push when rejected | 0.4, 0.3, 0.2 | `agents` |
| Import [tmux](https://github.com/tmux/tmux) 3.5a, 10,287 commits: `braid init` + `braid git import` vs `git clone --no-local --bare`; then checking it out too, with `braid workspace add` vs `git clone --no-local` | 17.6, 17.1 | `import` |
| Size after that import: `du` of `.braid/store` vs the bare clone after `git gc --aggressive` | 25.1 | `store` |
| Status of a tree of 100,000 files, clean: `braid status` vs `git status`, without the untracked cache, then with `core.untrackedCache=true` | 2.6, 5.8 | `status` |
| The same, with 10 files modified | 3.0, 5.7 | `status` |
| Commit one changed file of that tree: `braid commit` vs `git add` + `git commit` | 5.0 | `commit` |
| Commit one new file in a directory of 20,000 files, 200 times: `braid commit` vs `git add` + `git commit` | 1.8 | `wide` |
| The same without a working copy: `braid change create --land` | 0.9 | `wide` |
| Newest 1,000 changesets of tmux: `braid log -n 1000` vs `git log --first-parent -n 1000 --format=%H` after `git gc --aggressive` | 1.7 | `history` |

Git retried each push 1.1, 2.5 and 5.2 times on average with 4, 8 and 16
agents; no braid agent retried, and nearly every braid landing was a
rebase. Since this run, braid stores deltas, cuts large files into chunks,
edits a large directory in place, walks the working copy in parallel, and
looks objects up through the store's indexes instead of mapping the whole
store first. The ratios show none of that, and leave out
`braid log -- <path>` and `braid blame`, which came later.

<details>
<summary>Why braid was slower at that commit</summary>

- **Importing and storing history.** Braid kept every version of a file
  whole, compressed on its own, where git stores most versions as a delta
  against another. The import converted and checked every object on one
  thread, and much of that time went to checking every entry name against
  the aliases of `.git`, with Unicode normalisation and case folding, each
  time a tree was read or built.
- **Status and commit on a large tree.** `braid status` walked the working
  copy one directory at a time on one core, opening each directory from the
  root one name at a time, so that it never follows a symlink; it also read
  the whole base tree and rewrote the whole stat cache every time.
  `git status` spreads its checks over several cores and, with the
  untracked cache, skips the directories that did not change.
  `braid commit` has no staging area, so it ran a whole status first.
- **A new file in a wide directory.** Braid wrote only the few nodes of a
  large directory that change, but read and rebuilt the whole directory in
  memory to find them. From 2,500 to 20,000 files, the cost of one commit
  grew 5.2 times for `braid commit`, 3.6 times for `braid change create`
  and 3.5 times for git.
- **Opening the store.** Every command first built a map of where each
  object of the store is, which on a history the size of tmux's was a
  large part of `braid log -n 1000`; git looks each object up in its pack
  index when it needs it.

</details>

## Part of Elffuss

Braid is part of [Elffuss](https://elffuss.com), a family of tools for
building software with agents:
[elffuss.com](https://elffuss.com) ·
[Code](https://code.elffuss.com) ·
[Claw](https://claw.elffuss.com) ·
[Translator](https://translator.elffuss.com) ·
[Copilot](https://copilot.elffuss.com) ·
[Blog](https://elffuss.com/blog/) ·
[Hugging Face](https://huggingface.co/KikoCis) ·
[Dev log](https://bitacora.utopiaia.com/).
Braid has a [Space on Hugging Face](https://huggingface.co/spaces/KikoCis/elffuss-braid)
too.

- [elffuss](https://github.com/KikoCis/elffuss): the shared browser-agent core behind Code and Claw.
- [elffuss-code](https://github.com/KikoCis/elffuss-code): an IDE with an AI agent that runs entirely in your browser ([Space](https://huggingface.co/spaces/KikoCis/elffuss-code)).
- [elffuss-claw](https://github.com/KikoCis/elffuss-claw): an agentic OS in your browser, with apps generated on demand ([Space](https://huggingface.co/spaces/KikoCis/elffuss-claw)).
- [elffuss-engine](https://github.com/KikoCis/elffuss-engine): an LLM runtime that runs GGUF models on WebGPU.
- [elffuss-translator](https://github.com/KikoCis/elffuss-translator): real-time conversation translation in the browser.

## Prior art and credits

- Anthropic, [Redesigning source control at Anthropic](https://www.anthropic.com/engineering/redesigning-source-control):
  changesets stored against the first parent, server-side merges, minimal
  branch state, immutable objects served by any cache tier, and testing
  after merging instead of a merge queue. The article credits many of its
  techniques to Sapling's server,
  [Mononoke](https://github.com/facebook/sapling/blob/main/eden/mononoke/README.md),
  and its Bonsai data model.
- [Jujutsu](https://github.com/jj-vcs/jj): the operation log and undo.
- [Sapling](https://sapling-scm.com): history served at scale, and compressing
  a file version against the previous one.
- [gitoxide](https://github.com/GitoxideLabs/gitoxide): the git bridge.
- [BLAKE3](https://github.com/BLAKE3-team/BLAKE3), [FastCDC](https://www.usenix.org/conference/atc16/technical-sessions/presentation/xia),
  [prolly trees](https://docs.dolthub.com/architecture/storage-engine/prolly-tree)
  and [imara-diff](https://github.com/pascalkuthe/imara-diff).

## Contributing and security

- [CONTRIBUTING.md](CONTRIBUTING.md): how to build, test and propose
  changes. Pull requests are welcome.
- [SECURITY.md](SECURITY.md): how to report a vulnerability privately.

## License

[Apache-2.0](LICENSE). Copyright 2026 UtopiaIA. See [NOTICE](NOTICE).

---

## En castellano

**Muchos agentes. Un solo main. Sin reintentos.** Braid es un sistema de
control de versiones escrito desde cero en Rust, para repositorios en los
que muchos agentes de IA aterrizan cambios a la vez. Cada agente hace
commit en su propio espacio de trabajo, y aterrizar en `main` es un paso
corto que dan por turnos: si un cambio se ha quedado atrás, braid le hace
rebase sobre lo que haya aterrizado antes, y un conflicto vuelve como
datos, sin mover `main`. `braid verify`, nuevo en la 0.1.1, ejecuta los
tests después de aterrizar, localiza con bisect el changeset que los rompió
y aterriza el cambio que lo deshace. Git se queda en los bordes, así que
GitHub y Gitea siguen funcionando.

Se inspira en las ideas que Anthropic publicó sobre Scion, su sistema
interno de control de versiones. Es una implementación independiente: no
contiene código de Anthropic y no tiene relación con Anthropic ni cuenta
con su respaldo.

Está en la versión 0.1, así que tu repositorio de git sigue siendo la
fuente de verdad. Para trabajar desde varias máquinas está
[BraidHub](https://braidhub.elffuss.com), el servicio alojado de braid: los
repositorios son privados salvo que su dueño diga lo contrario, y cada uno
puede tener un repositorio de GitHub como espejo, al que el hub sube como
fast-forward cada aterrizaje en su rama por defecto. Las
[fases](#status-and-roadmap) dicen qué llega después y qué pruebas tiene
que pasar cada una.

Para usarlo en un proyecto, pídeselo a tu agente:

```text
Instala braid en este proyecto y configúralo para agentes siguiendo https://braid.elffuss.com/agents.md (léelo entero, con curl -fsSL)
```

Lo instala con cargo, te avisa de lo que braid no ejecuta, como los hooks
de git, importa la historia de git del proyecto y, con `braid agents init`,
escribe en su `AGENTS.md` las reglas para que cada sesión trabaje en su
propio espacio de trabajo y aterrice sus cambios con braid. La web,
[braid.elffuss.com](https://braid.elffuss.com/?lang=es), también está en
castellano.
