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
- **Built for agents.** Every command has a JSON form, and a change can be
  created through the API without a working copy.

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
  full segment is sealed with a sorted index; compaction can merge indexes.
  Only files that can no longer change are memory-mapped.
- **Op log.** Branch heads and the history of operations live in an
  append-only log behind a short lock. Every record names the agent that wrote
  it, so `braid op log` shows who did what and `braid undo` can step back.
- **Publish order.** Objects, then their journal entries, then a barrier, then
  the op log record that refers to them. The op log never points at an object
  that is not durable.
- **Durability modes.** `ordered` (local default) may lose the last operations
  on power loss but is never inconsistent; `full` (server default) syncs fully
  before acknowledging.
- **Derived data** (generation numbers, ancestry skip pointers, changed-path
  filters, blame, git mappings) is stored as records keyed by their inputs, and
  can always be recomputed.

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
the base, ours and theirs, and the hunks. Braid is stricter than git about
structural conflicts: a file against a directory, two names that differ only in
case or Unicode normalisation, a directory removed under a new file, and the
same file renamed two ways.

**Invariants.** A branch only moves forward. A branch never contains a
conflict. No value is overwritten except by a change whose base already
contained it. An acknowledged landing is durable. Readers only see durable
heads.

## Workspaces

- One workspace per agent, all sharing one store. No staging area: tracked
  files are snapshotted as they are.
- A workspace is any directory holding a `.braid` *file*, local and never
  tracked, that names the repository and the workspace. The workspace's state
  lives in the repository, under `.braid/ws/<name>/`: `state` (the base
  changeset, the branch it lands on, its directory, the file system's name
  rules, a rebase in progress), `stat` (the stat cache) and `lock`. Every
  command that changes a workspace holds its lock, so two processes never
  change one workspace at once; different workspaces never wait for each
  other, except for the short transaction in which a landing moves a branch.
- New files that look like secrets (`.env*`, `*.pem`, `id_*`) or are larger than
  1 MiB need an explicit `braid track`.
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
- `braid commit` records the working copy as a changeset on the base.
  `braid land` puts the workspace's changesets on its branch in one op log
  transaction: the engine fast-forwards or rebases them onto the head, and the
  branch moves with a compare-and-swap. The working copy then follows, writing
  only the difference. On conflicts the branch does not move, the conflicts
  come back as data, and the command exits with status 2.
- `braid update` brings a workspace onto its branch's head, rebasing its own
  changesets without moving the branch. On conflicts, the working copy gets
  the head's tree with the change's clean paths and git-style markers for
  content conflicts; a rebase is then in progress, `braid resolve` settles the
  other conflicts, and the next `braid commit` records the resolution on the
  head, refusing while markers remain.
- A changeset can also be made without any working copy: `braid change
  create` takes the base, the new contents and the deletions, and can require
  what the base holds at each path first, so that an agent never overwrites
  an edit it did not see.
- Signatures record the time in UTC for now: the local time zone offset is not
  computed yet.

## Git bridge

- Built on [gitoxide](https://github.com/GitoxideLabs/gitoxide). Braid contains
  no code from git itself, which is GPLv2.
- Import keeps the raw bytes of commits, tags and non-canonical trees, and
  verifies each commit by exporting it again and comparing ids.
- Sync fetches and imports first, then pushes with a lease, as a fast-forward.
  Braid never force-pushes. The push itself runs the `git` binary as a
  subprocess, since gitoxide does not push yet.

## Server and caches

- HTTP, with immutable objects served by id so that a daemon, a regional cache,
  nginx or a CDN can all answer the same request.
- The daemon, the cache tier and the server implement the same API, so a client
  does not know which one answers.
- Writes go to the authority: one ordered landing task per branch.
- Background work (derived data, compaction, git export) runs under an I/O
  budget, so a small server's disk is never saturated.

## Testing

- **Deterministic simulation.** The core runs over clock, randomness, disk and
  network traits. Simulated agents, servers and git remotes run with injected
  faults: lost acknowledgements, crashes between any two writes, partitions.
  Every seed reproduces its run, and the invariants above are checked after
  every step against a simple reference model.
- **Crash tests.** Failpoints and repeated `kill -9` of real processes, plus a
  file system shim that drops unsynced writes.
- **Properties.** Encodings round-trip canonically; incremental tree updates
  equal building from scratch; disjoint landings commute; a changeset's change
  list equals the diff of its trees.
- **Fuzzing** of every decoder, the git importer, path validation and merges.
- **Differential tests with git as the oracle** on real histories: tree diffs,
  path history, merge bases and three-way merges. Braid merging cleanly where
  git reports a conflict is treated as a critical bug.
- **Round trips** of real public repositories through import and export, bit
  for bit.

## Performance targets

Stated as complexity and as ratios against git on the same machine. Braid's
published benchmarks will only ever be ratios with the script that reproduces
them.

- Commit and land: proportional to the changed paths times the tree depth.
- `log -- path`: proportional to the changes that touched the path.
- Ancestry and merge base: logarithmic, using skip pointers and generation
  numbers.
- A lazy sparse clone: proportional to the objects under the requested paths.
- Store size: at most twice a garbage-collected git repository of the same
  history.
