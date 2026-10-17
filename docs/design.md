# Design

Braid is a version control system for repositories where many agents create
and land changes at the same time. This document explains the architecture and
the reasons behind it. The byte-level format is in
[`spec/format-v1.md`](spec/format-v1.md).

## Goals

- **Cost follows the change, not the repository.** Creating, rebasing and
  landing a change costs time proportional to the files it touches.
- **Many writers, no repository-wide lock.** Agents create changes in parallel;
  the only serial step is moving a branch, and it is short.
- **Correct without a daemon.** Any number of `braid` processes, of any version
  that understands the format, can share one store. A daemon may speed things
  up but is never needed for correctness.
- **Immutable, content-addressed data.** Any cache can serve any object, and
  nothing is ever rewritten in place.
- **Git at the edges.** Existing repositories come in with their full history,
  and GitHub or Gitea keep working as mirrors.
- **Built for agents.** Every command has a JSON form, a change can be
  created through the API without a working copy, and `braid agents init`
  tells a project's agents, in its `AGENTS.md`, how to work through braid.

## Non-goals

- Being a drop-in replacement for the `git` command line.
- Offline-first distributed workflows with many equal peers. Braid works
  offline on one machine, and a server is the authority when several machines
  share a repository.
- Preventing semantic conflicts: two changes can merge cleanly and still break
  the build together. Braid records what landed so CI can find out.

## Data model

- **Blob:** file content. **Tree:** a directory (flat, or split into
  content-defined nodes when large). **Changeset:** parents, the root tree, and
  the files that changed relative to the first parent.
- Ids are BLAKE3 hashes in key-derivation mode, one context per kind.
- A changeset stores both the per-file change list and the root tree id, and
  the store checks that they agree before accepting it. The change list makes
  history, blame and rebase cheap; the root tree id makes the changeset commit
  to the whole repository state.
- Large directories are split with a rule that depends only on entry names, so
  an edit rewrites a few small nodes instead of the whole directory, and equal
  contents always have equal ids.
- A changeset may carry extra fields, ordered key and value pairs that are
  part of its hash. Braid writes two: `agent`, the label of the agent that
  made the changeset, and `rebased-from`, the changeset a rebased one comes
  from. A rebase keeps the original's other fields, so `agent` names the
  agent that made the change wherever it lands. The git bridge writes extra
  fields as commit headers.
- Ids are shown as their first 12 hex digits, and any unique prefix of six
  digits or more names an object; the lookup reads the fan-out bucket of
  each index that the prefix falls in, and a prefix that several objects
  share is an error that lists them.

## Store

A store is a directory of files that several processes share. Its correctness
rests only on guarantees every local file system gives: exclusive create,
appends protected by checksums, advisory locks that the kernel releases when a
process dies, atomic rename, and ordered syncs.

- **Segments.** Objects are appended to segment files. A writer leases an open
  segment with a non-blocking lock, or creates a new one. Segments are never
  truncated: after a crash, the next writer appends after the last valid record.
- **Journals and indexes.** Each open segment has a journal of fixed-size,
  checksummed entries, so readers find new objects without scanning payloads. A
  segment is sealed with a sorted index once it is full or its journal lists
  8,192 records, and a writer merges the indexes of sealed segments once 8 of
  them pile up, so that a lookup goes through few. A process keeps a map of
  ids only for the open segments, and reads an index as lookups need it, one
  fan-out bucket at a time, each entry checked by its own checksum: opening a
  store reads no index entry, whatever the store holds. Nothing is
  memory-mapped.
- **Deltas.** Writers say which object a new one is probably close to: the
  engine names the directory node a rebuilt one replaces and, for a merged
  file, the branch's version; a workspace commit names the file's previous
  blob; the git import names the first parent's blob or tree at the same
  path, and its raw commit; a new git map names the one it grew from. A blob
  under 4 MiB, a tree or a derived object is then stored as a zstd frame
  against that object when this takes at most a quarter of the bytes.
  Deltas are one level deep: a base is always stored whole, so a read
  applies at most one delta, never a chain. The price is size: each delta
  holds every change since its base, until a version is stored whole again.
- **Large files.** Blobs of 4 MiB or more are cut into content-defined chunks
  (FastCDC), each a blob of its own, so versions of a large file, such as a
  model's weights, share every chunk an edit did not touch, with no side
  system like LFS. A blob's id is still the hash of its whole content. Large
  blobs are written and read as streams, a chunk at a time.
- **Garbage collection** is not there yet: nothing is ever removed from a
  store. When it comes, it must keep every base a delta needs and every chunk
  a chunk list names. `braid stats` shows how a store's objects are stored,
  and `braid fsck` checks deltas and chunked blobs with everything else.
- **Op log.** Branch heads and the history of operations live in an
  append-only log behind a short lock. Every record names the agent that wrote
  it, so `braid op log` shows who did what and `braid undo` can step back. A
  record's description is written inside the transaction, once the outcome
  is known: a landing says how it went, which changeset landed as which, and
  how many paths it merged.
- **Publish order.** Objects and their journal entries, flushed in one batch,
  then the op log record that refers to them, flushed in a second. The op log
  never points at an object that is not durable, and an index never reaches
  the disk before the segment it points into.
- **Durability modes.** `ordered` (local default) may lose the last operations
  on power loss but is never inconsistent; where the system has a write
  barrier (`F_BARRIERFSYNC` on Apple systems), it keeps the order of writes
  with one instead of waiting for the drive to write out its whole cache.
  `full` (server default) syncs fully before acknowledging. Either way,
  inside its lock a transaction flushes the two batches above, however many
  files it wrote to.
- **Derived data** (generation numbers, ancestry skip pointers, changed-path
  filters, blame, git mappings) is stored as records keyed by their inputs, and
  can always be recomputed.

### Derived data

Everything that is not in a changeset is derived data: computed from
changesets, stored, and always recomputable. A derived value is what a named
function, at some version, gives for some inputs, and its key is
`BLAKE3-derive-key("elffuss-braid 2026 derived v1", function ‖ version ‖ input
ids ‖ argument)`. The encodings are in section 8 of the format spec.

- **Records.** A value is stored as an ordinary derived object whose payload
  is its key followed by its encoding, which each function versions on its
  own: a new encoding is a new version, so new keys. The object's id depends
  on the value, so the store keeps a *derived index* next to its segments,
  `derived/index`: an append-only file of fixed-size entries, a key and a
  record id, each with a checksum.
- **A cache, checked on use.** The index is never trusted: a record is used
  only when its payload starts with the key and its value decodes, and a
  value whose record is missing, damaged or unindexed is computed again.
  Losing the index, or part of it, costs only time.
- **Many processes, no database.** Values are deterministic functions of their
  inputs, so two processes that compute the same value store the same bytes
  under the same id; storing an object twice does nothing, and two entries
  for a key name the same record. Writers append to the index under an
  exclusive lock, at a multiple of the entry size, so a crash leaves at worst
  an entry that fails its checksum, and readers take no lock. Records are
  made durable before an entry names them.
- **Lazy.** A value is computed the first time a command needs it, with the
  values it depends on, and recorded when the command finishes. `braid
  derive` computes them ahead of time, after a large import for example.
- The git map is a derived record too, but a single one, found through the
  ref `braid/git-map` rather than the index.

## Landing on a branch

`land` takes a changeset `C` built on base `B` and moves a branch to include it.
Bases are always ids, never branch names, so nothing is resolved late.

1. If the request for `(branch, C)` already landed, return that result. A lost
   acknowledgement is retried safely.
2. `B` must be on the branch's first-parent chain.
3. If `B` is the head, fast-forward.
4. Otherwise rebase `C` onto the head, path by path: unchanged since `B` takes
   `C`'s value; changed on both sides goes through a three-way merge. The cost
   follows the size of `C`.
5. Write the new objects, append the landing record, sync, then publish the new
   head.

Locally this step runs under the op log lock; on a server, one task per branch
runs it in order, grouping several landings into one sync.

Conflicts are reported as data, not as markers in a file: the path, the kind,
what the base, the head (ours) and the change (theirs) have there, and the
hunks, with line numbers counted from 0 and the end excluded. The command line
names the sides from the caller's point of view: `base`, `branch` and
`workspace`. Braid is stricter than git about structural conflicts: a file
against a directory, two names that differ only in case or Unicode
normalisation, a directory removed under a new file, and the same file renamed
two ways.

**Invariants.** A branch only moves forward. A branch never contains a
conflict. No value is overwritten except by a change whose base already
contained it. An acknowledged landing is durable. Readers only see durable
heads.

## History

Three derived functions answer history queries without walking all of
history every time. The engine's `History` reads them through the derived
index, computes what is missing, and remembers what it read.

- **Commit graph data** (`graph`, per changeset): the parents, the
  generation (1 without parents, otherwise 1 more than the largest of the
  parents'), the height along first parents, and a skip pointer to the
  first-parent ancestor at the height that Bitcoin's block index picks for
  its skip pointers. Following skip pointers reaches any first-parent
  ancestor in a number of steps that grows with the logarithm of the
  distance. With them, `is_ancestor` and `first_parent_chain_contains` find a
  first-parent ancestor in logarithmic time, and every other walk stops
  below the generation it looks for, since an ancestor always has a smaller
  generation. `merge_bases` walks down from both sides in order of
  generation, marking what each side reaches; a changeset reached from both
  is a merge base unless it is below one already found, and the walk stops
  when everything left to visit is. The answers are those of the walking
  functions, which landing, merging, workspaces and git sync still use,
  since they are given only objects; property tests compare the two on
  random histories with octopus and criss-cross merges.
- **Changed-path filters** (`changed-paths`, per changeset): the first parent
  and a Bloom filter, with git's parameters (10 bits per entry, 7 hashes), of
  the paths the change list names and their parent directories. `braid log
  [rev] -- <path>...` lists, newest first, the changesets on the first-parent
  chain whose change list touches a path or anything below it, as `git log
  --first-parent -- <path>` does, without following renames. It reads one
  small record per changeset walked, and a change list only where the filter
  lets the changeset through: the ones that touched the path, and about one
  in a hundred of the others.
- **Blame** (`blame`, per changeset and path, where the changeset sets the
  file): the changeset that last changed each line, as runs. It is computed
  from the blame of the previous version of the file on the first-parent
  chain and a line diff between the two versions, imara-diff's Myers
  algorithm with its indentation heuristic, which is what git's blame uses by
  default; a merge on the first-parent chain owns the lines it brought in.
  Blaming a file computes the records of the versions that lack one, back to
  the newest that has one, so blaming it again, at the same revision or a
  later one, costs one diff per new version. `braid blame <rev> <path>`
  prints the short id, the agent or the author, the date, the line number
  and the line; `--json` gives the same per line.

Blame differs from git's on purpose in two ways, and the differential tests
check that it differs in no other:

- **Renames are not followed.** Git follows a file across whole-file renames,
  and has no switch to turn that off. Braid's history of a path starts where
  the path last appeared: a line that git traces back across a rename is the
  changeset's that created the path.
- **The two diffs can differ.** Git's blame and braid's both use Myers'
  algorithm with an indentation heuristic, but not the same implementation.
  When a change has several shortest line diffs, they may keep different
  copies of lines that repeat, such as closing braces and blank lines. And
  neither always finds a shortest diff: both give up on it where the search
  would cost too much, and imara-diff's preprocessing can miss it too, so on
  a few changes one inserts and deletes more lines than the other. Blame
  then gives those lines to different changesets. On this repository's own
  history, fewer than one line in a thousand differs this way. Where the
  blames differ, the tests walk the versions of the file: replaying git's
  own diffs must give git's blame line for line, braid's blame at every
  version must be braid's own diff applied to its blame at the version
  before, and braid's diff must never insert fewer lines than the shortest
  diff (`git diff --minimal`).

## Workspaces

- One workspace per agent, all sharing one store. No staging area: tracked
  files are snapshotted as they are.
- A workspace is any directory holding a `.braid` *file*, local and never
  tracked, that names the repository and the workspace. It goes in
  `.braid/work/<name>` of the repository unless `workspace add` is given a
  directory, and it tracks a branch the repository has: a branch that does
  not exist would give an empty checkout whose first landing starts a new
  history under that name, so it is refused unless the repository has no
  branch yet or `--new-branch` asks for it.
- Every workspace also holds a `.git` *file* whose `gitdir:` line points
  nowhere. A workspace inside a git checkout, as `.braid/work/` is, would
  otherwise let git run in it find that checkout's repository, and `git
  stash` or `git reset --hard` there would act on the shared checkout's
  files. With the file, git stops with "not a git repository" instead. The
  walk skips `.git`, as it skips `.braid`, so neither is ever committed.
  `braid cat-object <base>:<path>` gives what git would have for a review
  or an undo: the file as the workspace's base has it.
- The workspace's state lives in the repository, under
  `.braid/ws/<name>/`: `state` (the base changeset, the branch it lands on,
  its directory, the file system's name rules, a rebase in progress),
  `stat` (the stat cache) and `lock`. Every command that changes a
  workspace holds its lock, so two processes never change one workspace at
  once; different workspaces never wait for each other, except for the
  short transaction in which a landing moves a branch.
- New files that look like secrets (`.env*`, `*.pem`, `id_*`) or are larger than
  1 MiB need an explicit `braid track`. Until then `braid commit` refuses,
  naming them, and `--force` leaves them out.
- Status uses a stat cache (modification time, size, inode, type, blob id),
  with the usual guard: a file whose modification time is not strictly older
  than the start of the scan that cached it, read from the workspace's own
  file system clock, is hashed again. Files the cache cannot vouch for are
  hashed in parallel. `.gitignore` and `.braidignore` are honoured; tracked
  files count whatever they say. A daemon can add a file system watcher.
- Checkouts never follow symlinks while walking or writing: every step opens
  one name from its parent's directory handle, refusing symlinks. They write
  through a temporary file and a rename, with modes 0644 or 0755 only, remove
  before they create when a path changes type, write symlink targets as data,
  and make gitlinks empty directories. Each workspace finds out once, with a
  scratch file, whether its file system ignores letter case or Unicode
  normalisation; names that would collide there, or that the platform cannot
  hold, are refused before anything is written. Untracked files in the way
  stop a checkout too. Nothing from repository content is ever run.
- `braid commit` records the working copy as a changeset on the base. It
  refuses while something blocks it, listing every reason at once: conflict
  markers left in a file, a conflict that is not about content and not marked
  resolved, a new file waiting for `braid track`. Before any of them it needs
  someone to sign as, from `--author`, `BRAID_AUTHOR` or `.braid/config`, as
  `land` and `update` do. `braid status` lists the same reasons as
  `blocking`, with `ready_to_commit`, and a missing or invalid author first,
  read the same way; it also counts how far the branch's head is ahead of
  the workspace along first parents, stopping after 5000 changesets, and
  lists the changesets that have not landed, including those of a rebase in
  progress.
- `braid land` puts the workspace's changesets on its branch in one op log
  transaction: the engine fast-forwards or rebases them onto the head, and the
  branch moves with a compare-and-swap. The working copy then follows, writing
  only the difference. On conflicts the branch does not move, the conflicts
  come back as data, and the command exits with status 2. `land` and `update`
  report the same fields: where each changeset went, from its id to the one
  that holds it now, the paths merged, the paths of the working copy written
  or removed, and on conflicts the command to run next.
- `braid update` brings a workspace onto its branch's head, rebasing its own
  changesets without moving the branch. On conflicts, the working copy gets
  the head's tree with the change's clean paths and git-style markers for
  content conflicts; a rebase is then in progress, `braid resolve` settles the
  other conflicts, and the next `braid commit` records the resolution on the
  head, refusing while markers remain. The resolution keeps the rebased
  changeset's author, extra fields and message, unless `-m` gives another.
- A changeset can also be made without any working copy: `braid change
  create` takes the base, the new contents and the deletions, and can require
  what the base holds at each path first, so that an agent never overwrites
  an edit it did not see.
- Signatures record the time with the offset of the local time zone, which
  [jiff](https://github.com/BurntSushi/jiff) reads from `TZ` or the system's
  setting without calling into the C library; when it cannot be determined,
  the offset is zero.

### Instructions for agents

Coding agents learn a project's rules from its `AGENTS.md`, or, for Claude
Code and Gemini CLI, from a `CLAUDE.md` or a `GEMINI.md` that can include
it. `braid agents init` writes there what an agent needs to work through
braid, so that a project set up once tells every later session, whatever
harness runs it. The guide that sets a project up is part 1 of the
repository's `AGENTS.md`, which the site also serves as `agents.md`. It
installs braid from the tag of the version it describes, since the site
serves it apart from the code.

- **A section of the file, not the file.** The rules sit between a line
  `<!-- braid:start -->` and a line `<!-- braid:end -->`, appended to the
  file the first time and rewritten in place after that. Text outside them
  is never touched, a second run changes nothing, and markers that do not
  pair up are refused before anything is written. Marker lines inside
  fenced code blocks are examples, not markers, and a file that ends inside
  an open block, where the section would read as code, is refused; a byte
  order mark before the first line stays where it is. A `CLAUDE.md` or a
  `GEMINI.md` that does not include `AGENTS.md` outside code gets the line
  `@AGENTS.md`, and nothing else.
- **Files written whole, or not at all.** Every check comes before any
  write, and the same checks run with `--dry-run`: a name that is not a
  regular file, a symbolic link out of the project, where other projects
  may read the file, a read-only file, and a file with hard links other
  than the project's guides are refused. Each file is written to a
  temporary file next to it, all of them before the first is renamed into
  place, so a failure while writing leaves every file as it was; one with
  hard links, `CLAUDE.md` and `AGENTS.md` as one file, say, is written in
  place, so that its names stay one file. A symbolic link stays one: the
  file it points to is the one written, and the JSON names it.
- **Only where braid is set up.** The section is committed, so every clone
  of the project has it, including clones where braid is not set up: CI,
  other machines, other people's agents. Its first rule says that a
  checkout without a `.braid` directory does not use braid, and that braid
  is set up there only when the user asks.
- **Workspaces under `.braid/work/`.** Each session creates its workspace
  at `.braid/work/<name>`, where `workspace add` puts it by default: inside
  the project, where a harness that only lets an agent write in the
  project can reach it; inside `.braid/`, which the setup has git ignore,
  so that the shared checkout's git never sees the workspaces; and gone
  with `.braid/` when braid is taken out. The setup's exclude line is
  `.braid`, which also covers the `.braid` file of a workspace made
  anywhere else in the checkout, since that file names the repository by
  its absolute path. Git does not run in a workspace (see
  [Workspaces](#workspaces)), and the section says why, and what to use
  instead.
- **The branch.** The section names the branch agents land on: the one
  `--branch` gives, which the repository must have; else the one the
  section already names, if the repository has it, so that a later run
  keeps it; else, inside a workspace, the one it tracks; else `main`,
  `master` or the repository's only branch. A branch the section names but
  the repository does not have was named before the project's history came
  in, or has been removed since: it is chosen again, and the command says
  so. With several branches and none of those, or with no repository or
  branch yet, the command asks for `--branch` rather than guess. The name
  goes into shell commands and is read back from the section, so it takes
  only characters that need no quoting.
- **Every command names its agent.** A harness may run each command in a
  new shell, which keeps neither variables nor the current directory, so
  the section passes `--agent` on every command that records an operation
  or a changeset, sync and undo included, and `--workspace` lets them run
  from the project root. The hints braid prints, and `next` in its JSON,
  repeat the `--repo`, `--agent` and `--workspace` typed on the command
  line, so that a session that follows them stays the same session.
- **What landed, and where.** A landing moves braid's branch only: the
  shared checkout and the git remote get it from `braid git sync` and
  `git pull --ff-only`. The section has a session that finishes say so to
  the user, and offer to run them, rather than leave the user looking at
  files that did not change; where the project has no git remote, it says
  so, and offers to make a bare one next to the project first. When the
  sync stops on conflicts, the section says that the merge is made with
  git, in the user's checkout, and points to the guide's steps, as the
  text `git sync` prints does.
- **Nobody signs by guesswork.** The guide's setup writes `.braid/config`
  from git's `user.name` and `user.email`, and stops before it imports
  anything while either is missing, rather than let every commit and
  landing fail later with "no author". Its own commit runs git with
  `user.useConfigOnly`, so that git refuses rather than sign with a name
  and an address made up from the machine's user and host, which the
  first sync would publish. Its check for a finished setup asks braid's
  copy of the branch for the section, so that a setup that stopped half
  way is run again rather than taken for done.

## Git bridge

- Built on [gitoxide](https://github.com/GitoxideLabs/gitoxide). Braid contains
  no code from git itself, which is GPLv2.
- Import keeps the raw bytes of commits, tags and non-canonical trees, and
  verifies each commit by exporting it again and comparing ids.
- Export is deterministic. What it generates for changesets made in braid
  (their commits, trees and blobs) enters the git map, so importing those
  commits back, from wherever they went, gives the same changesets.
- Braid's refs keep to their own namespaces: `braid/` for its bookkeeping
  (the git map is `braid/git-map`), `tags/`, `git/` for other git refs, and
  `remote/<label>/<branch>` for where the branches of git remotes were last
  seen. A git branch named like one of them is imported under its full name,
  below `git/`.
- Sync fetches and imports first, then pushes with a lease, as a fast-forward:
  the remote's history is never rewritten. The push itself runs the `git`
  binary as a subprocess, since gitoxide does not push yet.

### Sync

`braid git sync <remote> [--branch main] [--remote-branch <name>]` keeps a
braid branch and a branch of a git remote (GitHub, Gitea, a path) in step,
so that the git host stays the public face of a repository while agents land
in braid.

- **Mirror.** Each remote has a bare mirror at `.braid/git/<label>.git`. The
  label is the last name of the remote's path without `.git`, with every
  character other than ASCII letters, digits, `.`, `_` and `-` turned into
  `-`, then a `-` and the first 12 hex digits of the BLAKE3 hash of the
  address without user name, password, trailing `/` or `.git`. So
  `https://example.org/team/project.git` is `project-` and a hash, and no
  credential ever reaches a file or ref name. In the mirror,
  `refs/remotes/origin/<branch>` is the remote branch as fetched and
  `refs/heads/<branch>` the braid branch as exported. `.braid/git/<label>.lock`
  makes syncs with one remote take turns.
- **Fetch and import.** The remote branch is fetched with an explicit refspec
  and no tags, then imported. The braid ref `remote/<label>/<branch>` records
  it.
- **Reconcile.** With B the braid branch and G the remote branch: if they are
  equal, there is nothing to do; without B, the braid branch is created at G;
  if B is an ancestor of G, braid fast-forwards; if G is an ancestor of B, or
  there is no G, B is pushed. Otherwise braid merges them with the engine's
  `merge`, which follows landing's rules and conflicts: a clean merge moves
  the braid branch to the merge, which is then pushed; conflicts move nothing
  and exit with status 2. The braid branch moves only with a compare and swap,
  in one operation with the git map and the remote's ref.
- **Push.** `git push --atomic --porcelain
  --force-with-lease=refs/heads/<branch>:<fetched> <remote>
  <commit>:refs/heads/<branch>`, where `<fetched>` is what the same sync
  fetched. What is pushed descends from it, so the remote only fast-forwards.
  If someone else pushed in between, the lease fails and braid records
  nothing more (after a merge, its branch is already at the merge); the next
  sync merges the new commits.
- **Crashes.** Each sync first records in the git map the commits of the
  braid branch's changesets, which exports make deterministically. After a
  crash between a push and its record, the commits that come back from the
  remote are recognised as the changesets they came from, and nothing is
  imported twice.
- **Git commands** run with the user's environment, so credentials and SSH
  settings work, minus the variables that would point git at another
  repository, and with hooks, automatic garbage collection and the `ext::`
  transport turned off.
- **Identity.** A merge made by sync is committed by the `--agent` label, with
  the `email` that `.braid/config` sets (`email = ...`), or else
  `<agent>@example.org`. Git's own configuration is never read for identity.
- **Dry run.** `--dry-run` fetches into a temporary repository that borrows
  the mirror's objects, keeps braid objects in memory, and writes nothing.

## Server and caches

- HTTP, with immutable objects served by id so that a daemon, a regional cache,
  nginx or a CDN can all answer the same request.
- The daemon, the cache tier and the server implement the same API, so a client
  does not know which one answers.
- Writes go to the authority: one ordered landing task per branch.
- Background work (derived data, compaction, git export) runs under an I/O
  budget, so a small server's disk is never saturated.

## Testing

- **Deterministic simulation** (`crates/sim`). Simulated agents commit and
  land at once through a lander that does what the CLI does inside its op log
  transaction, with injected faults: lost acknowledgements, duplicate and
  late resubmissions, and lander crashes before the compare-and-swap. It runs
  on memory and on the real store. Every seed reproduces its run, and the
  landing invariants are checked after every step against an independent
  reference model.
- **Crash tests.** Failpoints and repeated `kill -9` of real processes sharing
  one store, and a power-loss model that drops whatever was not flushed before
  each flush batch.
- **Properties.** Encodings round-trip canonically; incremental tree updates
  equal building from scratch; disjoint landings commute; a changeset's change
  list equals the diff of its trees; three-way merges keep each side's changes
  and are symmetric; ancestry with derived data answers what the walks answer
  on random histories; changed-path filters never miss a path they hold;
  derived records do not depend on the order they are computed in.
- **Fuzzing.** Five targets: tree and changeset decoding, name rules, the git
  map record, and git commit, tag and tree parsing. Anything that decodes must
  encode back to the same bytes. CI runs each for a minute.
- **Differential tests with git as the oracle** on fixtures, random histories
  and this repository's own: path history (`git log --first-parent --
  <path>`), merge bases (`git merge-base --all`) and blame (`git blame
  --first-parent`, with the two differences above checked as such).
- **Round trips** through import and export, bit for bit, of fixture
  repositories full of unusual git objects and of this repository's history,
  in CI; several public repositories were also round-tripped by hand.

## Performance targets

Stated as complexity and as ratios against git on the same machine. Braid's
published benchmarks will only ever be ratios with the script that reproduces
them.

- Opening a store: proportional to its number of segments, of up to 8,192
  objects each, not to its number of objects. A lookup reads one fan-out
  bucket of each index it goes through, the first time it needs it.
- Commit and land: proportional to the changed paths times the tree depth.
- `log -- path`: proportional to the changes that touched the path, plus one
  filter check per changeset walked.
- Ancestry and merge base: logarithmic for first-parent ancestors, using skip
  pointers; otherwise proportional to the changesets above the generation
  looked for. Derived data is computed once, the first time.
- Blame: one line diff per version of the file not blamed before.
- A lazy sparse clone: proportional to the objects under the requested paths.
- Store size: at most twice a garbage-collected git repository of the same
  history.
