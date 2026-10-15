# Braid format v1

**Status: frozen.** Sections 1 to 5 (ids, the codec, trees, changesets and
names) are implemented in `crates/format` and frozen: any change to hashed bytes
needs a new format version. Sections 6 and 7 describe the git mapping and the
on-disk storage, which are not hashed and can still change.

Every golden vector below is pinned by the Rust tests and recomputed by
[`check_vectors.py`](check_vectors.py), a second implementation written from
this document alone. It needs the reference `b3sum` tool:

```bash
python3 docs/spec/check_vectors.py
```

This document covers what is **hashed**: ids, trees, changesets and names. How
the store keeps objects on disk (records, indexes, compression, deltas, chunks)
is not part of any id and has its own, separately versioned, section at the end.

## 1. Conventions

- Integers are little-endian.
- `uv` is an unsigned LEB128 integer of at most 64 bits. Encoders write the
  shortest form; decoders reject overlong forms (such as `80 00`) and values
  that do not fit in 64 bits.
- `sv` is a signed integer, zigzag-mapped and then written as `uv`.
- `bytes` is a `uv` length followed by that many raw bytes.
- `[32]` is exactly 32 raw bytes: an object id.
- A decoder rejects any bytes left over after the last field.
- Names and paths are byte strings, not text. A path joins names with `/`.

## 2. Object ids (implemented)

An object id is BLAKE3 in key-derivation mode, with one context string per kind:

| Kind | Code | Context string |
|---|---|---|
| blob | 1 | `elffuss-braid 2026 blob v1` |
| tree | 2 | `elffuss-braid 2026 tree v1` |
| changeset | 3 | `elffuss-braid 2026 changeset v1` |
| chunklist | 4 | `elffuss-braid 2026 chunklist v1` |
| derived | 5 | `elffuss-braid 2026 derived v1` |

```
id = BLAKE3-derive-key(context(kind), payload)        32 bytes
```

- The context separates kinds, so the payload carries no type prefix and can be
  hashed while it streams in.
- A **blob id is always the hash of the complete file content**, however the
  store keeps it (whole, as a delta, or in chunks).
- The text form is 64 lowercase hex digits; the short form is the first 12.
  Parsers accept lowercase only, so equal ids always have equal text.

Golden vectors (pinned in `crates/format/src/id.rs`):

| Kind | Payload | Id |
|---|---|---|
| blob | empty | `ff9c212a7ef64fa38f0c411b3a68d523a20fd197ee3ba03fa4a44b7c5e99c958` |
| blob | `hello\n` | `6acd5cdf880a325d2df716699943aae586ad77d54fb2ed0c5a3f53c76fa56f89` |
| tree | empty | `46f1ab000b3145d2e28ff69f7570eb88065d76ef86905bb268699adb9c860c08` |
| changeset | empty | `1be995db81f244ac5ea57f1ba45d9f9da94ad4edd6dbfa0979c16101a34d04ef` |
| chunklist | empty | `f14231be43c0cc73e7c4850ec9d980e6952b56c57f7125d0c39d16874be2ab04` |
| derived | empty | `6f101e23c531e6e3959878774beefb475944b2f0200e52eafb37fd80f97df172` |

Anyone can check them with the reference [`b3sum`](https://github.com/BLAKE3-team/BLAKE3/tree/master/b3sum) tool:

```bash
printf 'hello\n' | b3sum --derive-key "elffuss-braid 2026 blob v1"
```

## 3. Trees

A tree describes one directory. Small directories are one flat node. Large ones
are split into a balanced tree of nodes whose boundaries depend only on the
entry names, so the same set of entries always produces the same root id, no
matter in which order it was edited.

```
u8   format = 1
u8   node_type            0 flat, 1 leaf, 2 internal

flat | leaf:
  uv   n
  n × entry               strictly ascending by name bytes

entry:
  bytes name
  u8    kind              0 file, 1 executable, 2 symlink, 3 tree, 4 gitlink
                          (5 is reserved for conflict entries in a later version)
  kind 0–2:  [32] blob id, uv size in bytes
  kind 3:    [32] tree id
  kind 4:    u8 algorithm (1 = SHA-1, 2 = SHA-256), raw git id (20 or 32 bytes)

internal:
  u8   level              1 for the nodes just above the leaves
  uv   n
  n × (bytes last_name, [32] child id, uv entry_count)
```

- A symlink's target is stored as a blob.
- File sizes live in the tree, so a listing or a virtual file system can report
  sizes without fetching any content.
- An empty directory is the tree with zero entries. It can be represented, but
  checkouts never create one from scratch.

**Splitting rule.** A directory with **1,024 entries or fewer must be one flat
node.** A larger one must be split:

1. Walk the sorted entries left to right, filling a leaf. After adding entry
   `e` to a leaf that now holds `c` entries, end the leaf if
   `c ≥ 32` and `u32le(H(0, e.name)[0..4]) < 2³² / 128`, or if `c = 512`.
2. Build the next level the same way over the children, using each child's
   `last_name`, with level numbers 1, 2, …, until one node remains: the root.

`H(level, name) = BLAKE3-derive-key("elffuss-braid 2026 tree-split v1", u8 level ‖ name)`.

Boundaries depend only on names, so changing a file's content never moves them,
and an edit never rewrites the leaves before it. Any way of updating a directory
must produce exactly the nodes of building it from scratch.

A reader checks a split directory by collecting its entries and building them
again: the root must come out identical. A flat node may hold at most 1,024
entries, a leaf or internal node at least 1 and at most 512, and an internal
node's children must be one level below it.

Golden vectors:

| Tree | Id |
|---|---|
| empty (`01 00 00`) | `3136b0f18562ee73b840ea348c54ae3c7e97e4cc3e547ea9dd03fa1cd8b384cf` |
| small: `README.md` (file, `hello\n`), `latest` (symlink to `README.md`), `run.sh` (executable, `#!/bin/sh\n`), `src` (the empty tree), `vendor` (SHA-1 gitlink, twenty `11` bytes) | `a6c6d54bb7663d576e8f7a702e0670927e22098d6c15c10475e06b51dc3d8c1b` |
| large: 5,000 files `f00000` … `f04999`, each the one-byte blob `x`; 30 nodes | `2c88a412664c1fd862c3487e1313a045a20fe4f7bdeaf1bb71e08564edc7dcce` |

## 4. Changesets

A changeset is one change to the repository: its parents, the root tree it
produces, and the list of files that changed relative to its **first** parent.

```
u8    format = 1
uv    parent_count
parent_count × [32] changeset id      parent[0] is the base of the change list
[32]  root tree id
u8    changes_mode                    0 inline, 1 external
  mode 0:  uv n, n × change
  mode 1:  [32] blob id of the encoded change list, uv n
author:     bytes name, bytes email, sv seconds since the Unix epoch, sv offset in minutes
committer:  same layout
bytes message                         raw bytes, no normalisation
uv    extra_count
extra_count × (bytes key, bytes value)    order and duplicates preserved
u8    git_origin                      0 none; 1 followed by [32] blob id of the raw git commit bytes

change:
  bytes path
  u8    op                            0 delete, 1 file, 2 executable, 3 symlink, 4 gitlink
  op 1–3:  [32] blob id, uv size
  op 4:    u8 algorithm, raw git id
```

Rules:

- The change list is **exactly** the set of paths whose entry differs from the
  first parent's tree (different kind or different id), sorted by path bytes,
  without duplicates and without no-op entries. A changeset with no parents is
  compared against the empty tree.
- Changes are per file. A directory replaced by a file is a delete for every
  file below it plus one set for the new file.
- A symlink cannot take a name that must not be a symlink (section 5).
- The list is stored inline when its encoding (`uv n, n × change`) is at most
  64 KiB, and otherwise as a blob holding exactly that encoding, so a huge
  import does not make every history walk read a huge object. A decoder rejects
  an inline list over the limit; the store rejects an external list that would
  have fit inline.
- **Admission check.** A store accepts a changeset only if applying its change
  list to `parent[0]`'s root tree yields exactly `root_tree`, every referenced
  object is present, and every parent was already accepted. The check costs
  time proportional to the change, not to the repository.
- A merge has two or more parents; its change list is still relative to
  `parent[0]`.
- `git_origin` points at the original commit bytes for changesets imported from
  git, so export can reproduce the commit bit for bit. The reference is part of
  the hash.

Golden vector: no parents; root = the empty tree; two changes, `README.md`
set to the file `hello\n` (6 bytes) and `src/lib.rs` set to the empty file;
author and committer `Ada Lovelace <ada@example.org>` at second 1,700,000,000
with offset +60; message `Initial import\n`; no extras; no git origin.

```
01 00 3136b0f18562ee73b840ea348c54ae3c7e97e4cc3e547ea9dd03fa1cd8b384cf
00 02 09 524541444d452e6d64 01 6acd5cdf880a325d2df716699943aae586ad77d54fb2ed0c5a3f53c76fa56f89 06
      0a 7372632f6c69622e7273 01 ff9c212a7ef64fa38f0c411b3a68d523a20fd197ee3ba03fa4a44b7c5e99c958 00
0c 416461204c6f76656c616365 0f 616461406578616d706c652e6f7267 80c49fd50c 78
0c 416461204c6f76656c616365 0f 616461406578616d706c652e6f7267 80c49fd50c 78
0f 496e697469616c20696d706f72740a
00
00
```

Id: `4f8d49034a95a2938d30d13bee5508100211d04e1c125ef567a0c41c27f777d1`.

## 5. Names

The store is permissive, close to what `git fsck` accepts; checkouts are
strict, per file system.

A name in a tree is rejected if it:

- is empty, longer than 4,096 bytes, or contains `/` or NUL;
- is `.` or `..`;
- aliases `.braid` or `.git` once these are removed or folded: the code points
  that HFS+ ignores (U+200C–U+200F, U+202A–U+202E, U+206A–U+206F, U+FEFF),
  trailing dots and spaces (NTFS), letter case, and Unicode normalisation (NFC);
  this includes the 8.3 short forms `BRAID~1` and `GIT~1`;
- is `.gitmodules`, `.gitignore`, `.gitattributes` or `.braidignore` with kind
  symlink.

When writing a working copy, braid also refuses names that would collide on
that file system (letter case, Unicode normalisation), never follows a symlink
while walking or writing, and never runs anything found in repository content.

## 6. Git mapping

- Every imported git object has a two-way mapping between its git id and its
  braid id, stored as derived records.
- Imported commits and tags keep their raw bytes (`git_origin`), which preserves
  header order, signatures, encodings, unknown headers and odd author lines.
- A git tree that cannot be regenerated exactly from the braid tree (unsorted or
  duplicate entries, non-canonical modes such as `100664`) keeps its raw bytes as
  well.
- Git sorts a directory entry as if its name ended in `/`; braid sorts by raw
  name bytes. Export reorders.
- During import, every commit is exported again and its git id compared with
  the original. Any difference stops the import.

These rules decide the braid ids of imported objects, so two importers that
follow them give the same ids. They are implemented in `crates/git`.

**Trees.** An entry's kind comes from the file type bits of its octal mode:
`100000` is a file, executable when the owner may execute it (so `100664` is a
file and `100775` an executable); `120000` a symlink, whose blob is the target;
`040000` a directory; `160000` a SHA-1 gitlink. Any other type stops the
import, and so does a name section 5 refuses, checked before anything the tree
names is read. Of entries with the same name, the braid tree keeps the last
written, which is the one a git checkout uses. Braid trees never hold an empty
subdirectory (only a root can be empty), so an entry for an empty directory, or
for a directory holding only empty ones, is left out; changes, being per file,
never mention one.

The *canonical* git tree of a braid tree lists its entries in git's order with
the modes `100644`, `100755`, `120000`, `40000` and `160000`, naming each
subdirectory by its own canonical git tree. A git tree equal to the canonical
git tree of its braid tree needs nothing more. Any other, including one that
lost duplicate names or empty directories, keeps its raw bytes in a blob,
recorded against its git id, since several git trees can share one braid tree;
a tree that names such a tree is not canonical either. The empty tree is read
as present even when git did not store it.

**Commits.** The changeset of a commit has: as parents, the changesets of the
`parent` headers that follow the first header, `tree`, in their order; as root,
the braid tree of `tree`; the change list against the first parent; the
message, which is every byte after the first empty line; no extras; and as git
origin the blob with the commit's bytes. Author and committer are the first
`author` and `committer` headers, read as `Name <email> seconds ±HHMM`:

- the name is everything before the first `<`, without surrounding ASCII
  whitespace, and the email everything between it and the next `>`;
- after the `>`, the first word is the time in seconds and the second the
  offset, `±HHMM` read as a decimal number and turned into minutes; a missing
  or unreadable value is 0;
- a line without a `<` followed by a `>` is all name, with no email and time 0;
  a missing header gives an empty signature.

Annotated tags keep their raw bytes in a blob and record the object they point
to. Replacement objects and grafts are ignored, and objects are read raw:
no filters, no line-ending conversion, no LFS. Shallow and SHA-256
repositories are refused for now.

**Commits made in braid.** A changeset without a git origin is exported as
`tree`, one `parent` per parent, `author` and `committer` written as
`Name <email> seconds ±HHMM`, then one header per extra in order, a value with
newlines continuing on lines that start with a space, an empty line and the
message unchanged. A signature holding `<`, `>`, a newline or NUL, an offset
beyond ±99 hours, or an extra that cannot be a header has no git form and is
refused.

An export records what it generated in the map, in the same tables as an
import: the commit of each changeset, each canonical tree and each blob. An
import that meets one of those commits again, from wherever it went, takes the
changeset it was exported from instead of making a new one with a git origin.
The map stays one to one: a pairing whose commit or changeset (tree, blob) is
already mapped is not recorded, and a braid tree keeps the canonical git tree
it has. Since exports are deterministic, the pairings of any changeset can be
recomputed at any time and recorded again.

**Map record.** The mapping is one derived record, versioned on its own:

```
"braid-git-map" u8 version = 1
uv n, n × ([20] commit, [32] changeset)
uv n, n × ([20] tree, [32] braid tree)
uv n, n × ([20] tree, [32] blob with its raw bytes)          trees that are not canonical
uv n, n × ([20] blob, [32] braid blob, uv size)
uv n, n × ([20] tag, [32] blob with its raw bytes, [20] target, u8 kind)
uv n, n × (bytes ref name, [20] tag)                         tag names
```

Each table is sorted by key without duplicates; target kinds are 1 commit,
2 tree, 3 blob and 4 tag. The reverse mappings are rebuilt on load.

## 7. Storage (not hashed, versioned on its own)

This is the on-disk layout of the local store as implemented in
`crates/store`. It is not hashed: it can change without changing any id.
Fixed-size integers are little-endian; `uv`, `sv` and `bytes` are the codec of
section 1.

```
.braid/store/format            object-format=blake3-dk-v1, then storage=<version>
.braid/store/format.lock       empty; locked exclusively to raise the storage version
.braid/store/seg/<name>.seg    object records, append-only
.braid/store/seg/<name>.jrn    journal of a segment that is still open
.braid/store/seg/<name>.idx    sorted index of a sealed segment
.braid/store/oplog/lock        empty; locked exclusively by transactions, shared by readers
.braid/store/oplog/log         operation records, append-only
.braid/store/oplog/ckpt.<seq>  every ref as of operation <seq>
```

Segment names are 32 random lowercase hex digits. Files whose names start with
`tmp-` are still being written, or were left by a writer that died; they are
never read, and are removed once they are a minute old and unlocked.

**Storage version.** The `format` file has two lines: the object ids
(`object-format=blake3-dk-v1`), and the storage version, what a reader must
understand to use the store (`storage=2`). A store without the second line has
storage version 1: records stored whole (codecs 0 and 1), which every version
of braid reads. Version 2 adds deltas (codec 3) and chunked blobs (codec 4). A
version of braid refuses a store of a later storage version, and a `format`
file with any line it does not know; versions from before storage versions
compare the whole file with the first line, so they refuse version 2 too.

New stores start at the current version. A store is raised only when a writer
is about to append the first record that needs a later version, or when a
handle that opens it finds such records already there. The writer takes an
exclusive lock on `format.lock`, reads `format` again, and if it is still
older publishes the new one through a temporary file and a rename: the version
never goes down, and it is durable before any record that needs it.

A journal entry or an index whose checksum holds but which names an object
kind or a codec the reader does not know, sets a reserved field, or has a later
index version, was written by a newer version of braid: it is not torn or
damaged. The reader refuses the store rather than skip the entry, scan around
the index, or append after them, so nothing is ever written over records it
does not understand.

**Sharing.** A writer appends only to a segment it holds an exclusive,
non-blocking lock on: an unlocked open segment if there is one, else a new one
made with an exclusive create. The kernel releases the lock when the writer
dies, and the next writer resumes after what the segment's journal lists.
Segments are never truncated, since others may be reading them. A segment past
the size threshold is sealed: flushed, given a sorted index through a temporary
file and a rename, and its journal removed; it is never written again.

**Publish order.** Objects are written and synced before the op log records an
operation that refers to them, so the op log never points at an object that is
not durable.

**Object record**, starting at a multiple of 8 bytes:

```
u32 sync = 0xB7A1D5E9 | u8 kind | u8 codec | u16 flags = 0
u32 stored_len | u64 raw_len | [32] id
[32] dictionary or base id, codecs 2 and 3 only
payload, stored_len bytes
u32 crc32c of everything from the sync word to the end of the payload
zero padding to a multiple of 8 bytes
```

`raw_len` is always the size of the object itself, whatever the codec, so the
kind and size of any object come from its header without reading its payload.
After a crash, the next writer appends at the next multiple of 8 after the end
of the file, so the remains of a cut append stay in place and scans step over
them.

| Codec | Payload | Used for |
|---|---|---|
| 0 raw | the object as it is (`stored_len == raw_len`) | payloads up to 64 bytes, and larger ones zstd does not make smaller |
| 1 zstd | one zstd frame, level 3 | other objects stored whole |
| 2 | reserved: zstd with a dictionary | not written yet |
| 3 delta | one zstd frame against the raw payload of the base the header names | blobs under 4 MiB and trees, given a similar object |
| 4 chunked | the chunk list | blobs only, of 4 MiB or more |

**Deltas (codec 3).** A writer may say which object a new one is probably
close to: the same path in the parent, for blobs and trees. A blob under 4 MiB
or a tree is then compressed with zstd at level 3, with the raw payload of the
base as a reference prefix (`ZSTD_CCtx_refPrefix`), a window that holds the
base and the object together (at most 2²⁷ bytes, so that any decoder accepts
it), and long-distance matching. The frame is kept only when it and the 32
bytes of the base id take at most a quarter of the bytes of the object stored
on its own (codec 0 or 1); otherwise the object is stored on its own.

The fraction decides how often a version is stored whole again, since a delta
holds every change since its base. Imports of the histories of four public
repositories took, as a ratio to the same histories packed by `git gc
--aggressive`, 2.9 to 3.7 keeping deltas up to a half, 2.6 to 3.3 up to a
third, 2.5 to 3.1 up to a quarter, and 2.6 to 3.1 up to a sixth.

Depth is always one. A base is stored whole, with codec 0 or 1, and when the
object named as similar is a delta, the new one is made against that delta's
own base. Reading a delta reads its base and applies one delta, never a chain.
A reader that knows several copies of one object, as racing writers can leave,
uses a copy stored whole before a delta, and a delta before a chunked blob, so
that a base always resolves to a copy stored whole when there is one.

**Chunked blobs (codec 4).** A blob of 4 MiB or more is cut with FastCDC, the
2020 algorithm with normalisation level 1 and the standard gear table, into
chunks whose boundaries depend only on the content:

| Blob size | Smallest chunk | Average | Largest |
|---|---|---|---|
| 4 MiB to 256 MiB | 16 KiB | 64 KiB | 256 KiB |
| 256 MiB or more | 256 KiB | 1 MiB | 4 MiB |

The last chunk may be smaller than the smallest size. A writer that streams a
blob in cuts it with the sizes for the size it expects, such as a file's
length; how a blob is cut never changes its id. Each chunk is a blob of its
own, whose id is the hash of its bytes, so a chunk shared by several files or
versions is stored once. A chunk is stored whole: raw when its first 64 KiB do
not compress below 0.9 of their size, else compressed when that is smaller. A
chunk list never names a chunked blob: a chunk is written whole even when a
chunked copy of the same bytes exists. The blob's own record holds its chunk
list:

```
uv n                                 n ≥ 1
n × ([32] chunk id | uv chunk size)  in order; sizes at least 1
```

The sizes add up to the record's `raw_len`, and the chunks, in order, make up
the blob. Its id is still the hash of its full content: readers fetch the
chunks one at a time, each checked against its own id and size, and check the
whole against the blob's id, so a blob of any size can be read and written as a
stream.

**What depends on what.** A delta needs its base, and a chunked blob its
chunks. A writer that finds a copy of an object instead of writing it again
relies on it only together with what it needs, and flushes, at its next sync,
the open segments of other writers that hold any of them, as it does for the
copy itself. There is no garbage collection yet (`braid gc` is planned): no
object is ever removed. Garbage collection must never drop a base while a delta
needs it, nor a chunk while a chunk list names it.

**Journal entry**, 64 bytes at a multiple of 64, written after its record:

```
[32] id | u64 offset | u32 stored_len | u8 kind | u8 codec | u16 0
u64 raw_len | u32 record crc | u32 crc32c of the 60 bytes before it
```

Entries that fail their CRC are skipped, and a new lease holder writes after the
last valid one.

**Index**, written once when a segment is sealed:

```
[8] "braididx" | u32 version = 1 | u32 0 | u64 count | u64 segment length
256 × u32 fan-out: entry b counts the ids whose first byte is at most b
count × [32] ids, strictly ascending
count × (u64 offset | u32 stored_len | u8 kind | u8 codec | u16 0
         u64 raw_len | u32 record crc | u32 0), in id order
[32] BLAKE3 hash of everything before it
```

Only entries whose record checks out go in. Of several copies of an object, a
copy stored whole wins over a delta, a delta over a chunked blob, and the first
of equals wins.

**Operation record**:

```
u32 magic = 0xB7A10910 | u32 body_len | body | u32 body_len + 16
u32 crc32c of everything before it

body:   u8 version = 1 | uv seq | sv milliseconds since the Unix epoch
        bytes agent | bytes description | uv seq undone, 0 for none
        uv n | n × (bytes ref name | u8 which | [32] old | [32] new)
```

`which` has bit 0 set when `old` is present and bit 1 when `new` is; absent
values take no bytes. The length at the end lets readers walk the log
backwards. Sequence numbers start at 1 and have no gaps. A record cut short by a
crash is skipped by readers and cut off by the next transaction, but only when
no whole record follows it.

**Checkpoint**, written through a temporary file and a rename every
`checkpoint_interval` operations; the newest two are kept:

```
[8] "braidckp" | u32 version = 1 | u32 0 | u64 seq | u64 where record seq ends
uv n | n × (bytes ref name | [32] id), names strictly ascending
[32] BLAKE3 hash of everything before it
```

**Planned, not implemented yet:** a binary fuse filter in indexes for fast
misses, merged indexes, per-repository dictionaries (codec 2), and garbage
collection (`braid gc`), which must keep every base and chunk that something
still needs.
