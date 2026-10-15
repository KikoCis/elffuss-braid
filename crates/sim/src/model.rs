// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The reference model: a deliberately simple, executable specification of
//! landing.
//!
//! A repository state is a map from path to file. Landing a change built on a
//! base looks at each path the change touches: if the head left the path as
//! it was in the base, the change's file is taken; if the head already has the
//! change's file, there is nothing to do; otherwise both sides changed it, and
//! only two regular files can still be merged, line by line. Around those
//! files come the structural rules of `docs/design.md`: a file against a
//! directory, a directory removed under a file added on the other side, and
//! names that differ only in letter case or Unicode normalisation.
//!
//! Everything here works on the flat map, with naive scans over it, and none
//! of it calls the engine. Tree ids come from [`Oracle`], which builds trees
//! from scratch with the format crate, as the engine's property tests do.
//!
//! The line merge expects what the simulation's edits guarantee: no line
//! appears twice in one version of a file, and lines never move. Under that
//! condition every line diff has a single answer, so the model can predict
//! the engine's merges exactly, conflicts and hunks included.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::rc::Rc;

use braid_format::{Change, ChangeOp, Entry, EntryKind, Kind, ObjectId, build_directory};
use unicode_normalization::UnicodeNormalization;

/// Content with a NUL byte among this many leading bytes is binary.
const BINARY_SNIFF_BYTES: usize = 8000;

/// What kind of file a value is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    /// A regular file.
    File,
    /// An executable file.
    Exec,
    /// A symlink; the content is its target.
    Link,
}

/// A file: its mode, its content, and the id of the content as a blob.
#[derive(Clone)]
pub struct Value {
    mode: Mode,
    content: Rc<[u8]>,
    blob: ObjectId,
}

impl Value {
    /// A file of this mode and content.
    pub fn new(mode: Mode, content: Vec<u8>) -> Self {
        let blob = ObjectId::hash(Kind::Blob, &content);
        Value {
            mode,
            content: content.into(),
            blob,
        }
    }

    /// Its mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Its content.
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// The same content with another mode.
    pub fn with_mode(&self, mode: Mode) -> Self {
        Value {
            mode,
            content: Rc::clone(&self.content),
            blob: self.blob,
        }
    }

    /// Whether it is a regular file, executable or not.
    pub fn is_regular(&self) -> bool {
        matches!(self.mode, Mode::File | Mode::Exec)
    }

    /// Whether it is a regular file whose content can be edited line by line.
    pub fn is_text(&self) -> bool {
        self.is_regular() && !is_binary(&self.content)
    }

    /// The tree entry for this file.
    pub fn entry(&self) -> EntryKind {
        let (blob, size) = (self.blob, self.content.len() as u64);
        match self.mode {
            Mode::File => EntryKind::File { blob, size },
            Mode::Exec => EntryKind::Executable { blob, size },
            Mode::Link => EntryKind::Symlink { blob, size },
        }
    }

    /// The change that sets a path to this file.
    pub fn op(&self) -> ChangeOp {
        ChangeOp::from_entry(&self.entry()).expect("a file is never a directory")
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode && self.blob == other.blob
    }
}

impl Eq for Value {}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self.mode {
            Mode::File => "file",
            Mode::Exec => "exec",
            Mode::Link => "link",
        };
        if is_binary(&self.content) {
            write!(f, "{mode} of {} binary bytes", self.content.len())
        } else {
            write!(f, "{mode} {:?}", String::from_utf8_lossy(&self.content))
        }
    }
}

/// A repository state: every file, by path. No path is inside another.
pub type Files = BTreeMap<Vec<u8>, Value>;

/// A change list in the model's terms: each changed path with its new file,
/// or `None` where the file is deleted, sorted by path.
pub type Edits = Vec<(Vec<u8>, Option<Value>)>;

/// Whether content is binary: a NUL byte among its first bytes.
pub fn is_binary(content: &[u8]) -> bool {
    content
        .iter()
        .take(BINARY_SNIFF_BYTES)
        .any(|byte| *byte == 0)
}

/// `dir/name`, or `name` when `dir` is the root.
pub fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = dir.to_vec();
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// The lengths of the proper prefixes of a path: 1 and 3 for `a/b/c`.
pub fn prefix_lengths(path: &[u8]) -> impl Iterator<Item = usize> + '_ {
    path.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(|(at, _)| at)
}

/// Whether `path` is inside the directory `dir`.
pub fn inside(dir: &[u8], path: &[u8]) -> bool {
    path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
}

/// Whether two paths are the same, or one is inside the other.
pub fn related(a: &[u8], b: &[u8]) -> bool {
    a == b || inside(a, b) || inside(b, a)
}

/// The files below the directory `dir`, which is not the root.
pub fn files_below<'a>(
    files: &'a Files,
    dir: &[u8],
) -> impl Iterator<Item = (&'a Vec<u8>, &'a Value)> + 'a {
    let mut from = dir.to_vec();
    from.push(b'/');
    let mut to = dir.to_vec();
    to.push(b'/' + 1);
    files.range(from..to)
}

/// Whether `dir` is a directory: some file is below it.
pub fn is_dir(files: &Files, dir: &[u8]) -> bool {
    files_below(files, dir).next().is_some()
}

/// What is at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found<'a> {
    /// This file.
    File(&'a Value),
    /// A directory.
    Dir,
    /// Nothing, and no prefix of the path is a file.
    Absent,
    /// Nothing, because the prefix of this many bytes is a file.
    Blocked(usize),
}

/// What is at `path`.
pub fn find<'a>(files: &'a Files, path: &[u8]) -> Found<'a> {
    if let Some(len) = prefix_lengths(path).find(|&len| files.contains_key(&path[..len])) {
        return Found::Blocked(len);
    }
    match files.get(path) {
        Some(value) => Found::File(value),
        None if is_dir(files, path) => Found::Dir,
        None => Found::Absent,
    }
}

/// Whether there is a file or a directory at `path`.
fn has_entry(files: &Files, path: &[u8]) -> bool {
    matches!(find(files, path), Found::File(_) | Found::Dir)
}

/// The names in the directory `dir` (the root when empty).
fn names_in(files: &Files, dir: &[u8]) -> BTreeSet<Vec<u8>> {
    let skip = if dir.is_empty() { 0 } else { dir.len() + 1 };
    let first_name = |path: &Vec<u8>| {
        let rest = &path[skip..];
        let end = rest
            .iter()
            .position(|byte| *byte == b'/')
            .unwrap_or(rest.len());
        rest[..end].to_vec()
    };
    if dir.is_empty() {
        files.keys().map(first_name).collect()
    } else {
        files_below(files, dir)
            .map(|(path, _)| first_name(path))
            .collect()
    }
}

/// The shortest proper prefix of `path` that does not exist, when every
/// shorter one is a directory.
fn first_missing_dir(files: &Files, path: &[u8]) -> Option<usize> {
    for len in prefix_lengths(path) {
        let prefix = &path[..len];
        if files.contains_key(prefix) {
            return None;
        }
        if !is_dir(files, prefix) {
            return Some(len);
        }
    }
    None
}

/// A name as file systems that ignore case and normalisation compare it:
/// Unicode NFC, lowercased letter by letter, NFC again. A name that is not
/// UTF-8 is lowercased as ASCII.
pub fn fold(name: &[u8]) -> Vec<u8> {
    if name.is_ascii() {
        // Normalisation leaves ASCII as it is.
        return name.to_ascii_lowercase();
    }
    match std::str::from_utf8(name) {
        Ok(text) => {
            let composed: String = text.nfc().collect();
            let lower: String = composed.chars().flat_map(char::to_lowercase).collect();
            lower.nfc().collect::<String>().into_bytes()
        }
        Err(_) => name.to_ascii_lowercase(),
    }
}

/// The edits that turn `old` into `new`, sorted by path.
pub fn change_list(old: &Files, new: &Files) -> Edits {
    let mut edits = Vec::new();
    let mut a = old.iter().peekable();
    let mut b = new.iter().peekable();
    loop {
        match (a.peek(), b.peek()) {
            (None, None) => return edits,
            (Some((path, _)), None) => {
                edits.push(((*path).clone(), None));
                a.next();
            }
            (None, Some((path, value))) => {
                edits.push(((*path).clone(), Some((*value).clone())));
                b.next();
            }
            (Some((old_path, old_value)), Some((new_path, new_value))) => {
                if old_path < new_path {
                    edits.push(((*old_path).clone(), None));
                    a.next();
                } else if new_path < old_path {
                    edits.push(((*new_path).clone(), Some((*new_value).clone())));
                    b.next();
                } else {
                    if old_value != new_value {
                        edits.push(((*new_path).clone(), Some((*new_value).clone())));
                    }
                    a.next();
                    b.next();
                }
            }
        }
    }
}

/// Applies edits: deletes, then sets.
pub fn apply_edits(files: &mut Files, edits: &Edits) {
    for (path, value) in edits {
        if value.is_none() {
            files.remove(path);
        }
    }
    for (path, value) in edits {
        if let Some(value) = value {
            files.insert(path.clone(), value.clone());
        }
    }
}

/// The engine's form of a change list.
pub fn to_changes(edits: &Edits) -> Vec<Change> {
    edits
        .iter()
        .map(|(path, value)| Change {
            path: path.clone(),
            op: value.as_ref().map_or(ChangeOp::Delete, Value::op),
        })
        .collect()
}

/// One region of a text that the two sides changed differently: a range of
/// lines and their text, in the base, ours and theirs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// The region in the base.
    pub base: (Range<usize>, Vec<u8>),
    /// What ours has instead.
    pub ours: (Range<usize>, Vec<u8>),
    /// What theirs has instead.
    pub theirs: (Range<usize>, Vec<u8>),
}

/// The result of merging two versions of a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextMerge {
    /// The merged text.
    Clean(Vec<u8>),
    /// The regions changed differently; none for binary content.
    Conflict(Vec<Hunk>),
}

/// Lines end after each `\n`; a last line without one is a line too.
fn lines(text: &[u8]) -> Vec<&[u8]> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split_inclusive(|byte| *byte == b'\n').collect()
}

/// One change of one side: base lines `base` replaced by side lines `side`.
#[derive(Clone, Debug)]
struct SideHunk {
    base: Range<usize>,
    side: Range<usize>,
}

/// The changes that turn `base` into `side`. Lines must be distinct within
/// each, and the lines both have must be in the same order; the diff then has
/// a single answer: every shared line is kept.
fn side_hunks(base: &[&[u8]], side: &[&[u8]]) -> Result<Vec<SideHunk>, String> {
    let in_base: HashSet<&[u8]> = base.iter().copied().collect();
    let in_side: HashSet<&[u8]> = side.iter().copied().collect();
    if in_base.len() != base.len() || in_side.len() != side.len() {
        return Err("a line appears twice in one version".to_owned());
    }
    let (mut i, mut j) = (0, 0);
    let mut hunks = Vec::new();
    while i < base.len() || j < side.len() {
        if i < base.len() && j < side.len() && base[i] == side[j] {
            i += 1;
            j += 1;
            continue;
        }
        let (i0, j0) = (i, j);
        while i < base.len() && !in_side.contains(base[i]) {
            i += 1;
        }
        while j < side.len() && !in_base.contains(side[j]) {
            j += 1;
        }
        let aligned = match (base.get(i), side.get(j)) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        };
        if !aligned || (i, j) == (i0, j0) {
            return Err("a line moved between versions".to_owned());
        }
        hunks.push(SideHunk {
            base: i0..i,
            side: j0..j,
        });
    }
    Ok(hunks)
}

/// Merges `ours` and `theirs`, two versions of `base`, line by line.
///
/// Changes of the two sides whose base ranges overlap or touch, counting an
/// insertion at the edge of a change, form one region; a region changed by
/// one side takes that side's lines, one changed identically by both takes
/// them once, and any other is a conflict. Binary content merges only when
/// one side kept the base or both agree.
///
/// Fails when the inputs break the model's assumptions: a repeated line, or a
/// line that moved.
pub fn merge_text(base: &[u8], ours: &[u8], theirs: &[u8]) -> Result<TextMerge, String> {
    if ours == theirs || theirs == base {
        return Ok(TextMerge::Clean(ours.to_vec()));
    }
    if ours == base {
        return Ok(TextMerge::Clean(theirs.to_vec()));
    }
    if is_binary(base) || is_binary(ours) || is_binary(theirs) {
        return Ok(TextMerge::Conflict(Vec::new()));
    }
    let (b, o, t) = (lines(base), lines(ours), lines(theirs));
    let sides = [side_hunks(&b, &o)?, side_hunks(&b, &t)?];

    // Every change of either side, by where it starts in the base; ours
    // first on a tie.
    let mut order: Vec<(usize, usize, usize)> = Vec::new();
    for (side, hunks) in sides.iter().enumerate() {
        for (index, hunk) in hunks.iter().enumerate() {
            order.push((hunk.base.start, side, index));
        }
    }
    order.sort();

    // Regions: a change joins the region before it when it starts at or
    // before the region's end.
    struct Region {
        start: usize,
        end: usize,
        members: [Vec<usize>; 2],
    }
    let mut regions: Vec<Region> = Vec::new();
    for (start, side, index) in order {
        let end = sides[side][index].base.end;
        match regions.last_mut() {
            Some(region) if start <= region.end => {
                region.end = region.end.max(end);
                region.members[side].push(index);
            }
            _ => {
                let mut members = [Vec::new(), Vec::new()];
                members[side].push(index);
                regions.push(Region {
                    start,
                    end,
                    members,
                });
            }
        }
    }

    let texts = [&o, &t];
    let mut out = Vec::new();
    let mut hunks = Vec::new();
    let mut copied = 0;
    // Lines each side has gained before the current region.
    let mut shift = [0isize; 2];
    for region in &regions {
        let mut ranges = [0..0, 0..0];
        for side in 0..2 {
            let growth: isize = region.members[side]
                .iter()
                .map(|&index| {
                    let hunk = &sides[side][index];
                    hunk.side.len() as isize - hunk.base.len() as isize
                })
                .sum();
            let start = region.start as isize + shift[side];
            let end = region.end as isize + shift[side] + growth;
            ranges[side] = start as usize..end as usize;
            shift[side] += growth;
        }
        for line in &b[copied..region.start] {
            out.extend_from_slice(line);
        }
        copied = region.end;
        let changed = [!region.members[0].is_empty(), !region.members[1].is_empty()];
        let take = |side: usize, out: &mut Vec<u8>| {
            for line in &texts[side][ranges[side].clone()] {
                out.extend_from_slice(line);
            }
        };
        match changed {
            [true, false] => take(0, &mut out),
            [false, true] => take(1, &mut out),
            _ if o[ranges[0].clone()] == t[ranges[1].clone()] => take(0, &mut out),
            _ => {
                let text = |lines: &[&[u8]]| lines.concat();
                hunks.push(Hunk {
                    base: (region.start..region.end, text(&b[region.start..region.end])),
                    ours: (ranges[0].clone(), text(&o[ranges[0].clone()])),
                    theirs: (ranges[1].clone(), text(&t[ranges[1].clone()])),
                });
            }
        }
    }
    for line in &b[copied..] {
        out.extend_from_slice(line);
    }
    Ok(if hunks.is_empty() {
        TextMerge::Clean(out)
    } else {
        TextMerge::Conflict(hunks)
    })
}

/// A conflict as the model sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelConflict {
    /// Where it is.
    pub path: Vec<u8>,
    /// The engine's code for its kind: `CONTENT`, `MODIFY_DELETE`, `MODE`,
    /// `DF`, `DIR_REMOVED` or `CASE`.
    pub code: &'static str,
    /// The removed directory of a `DIR_REMOVED`, the name already on the
    /// head of a `CASE`; empty otherwise.
    pub detail: Vec<u8>,
    /// The conflicting regions of a text `CONTENT` conflict.
    pub hunks: Vec<Hunk>,
}

impl ModelConflict {
    fn new(path: &[u8], code: &'static str) -> Self {
        ModelConflict {
            path: path.to_vec(),
            code,
            detail: Vec::new(),
            hunks: Vec::new(),
        }
    }
}

/// A text merge the model made while rebasing, kept so that it can be
/// compared with the engine's `merge3` on the same inputs.
#[derive(Clone, Debug)]
pub struct MergeCase {
    /// The file.
    pub path: Vec<u8>,
    /// The base text: empty when the base had no regular file there.
    pub base: Vec<u8>,
    /// The head's text.
    pub ours: Vec<u8>,
    /// The change's text.
    pub theirs: Vec<u8>,
    /// What the model made of them.
    pub result: TextMerge,
}

/// What landing a change on a head that moved since its base gives.
#[derive(Clone, Debug)]
pub enum Rebase {
    /// The change lands.
    Clean {
        /// The new state.
        state: Files,
        /// The change list against the head; empty when the head already
        /// has everything the change does.
        resolved: Edits,
        /// The paths both sides changed and that were merged.
        merged: Vec<Vec<u8>>,
    },
    /// The change conflicts with the head.
    Conflicts(Vec<ModelConflict>),
}

/// Rebases the change that turns `base` into `theirs` onto `ours`, a head
/// that descends from `base`. Text merges are added to `merges`.
pub fn rebase(
    base: &Files,
    ours: &Files,
    theirs: &Files,
    merges: &mut Vec<MergeCase>,
) -> Result<Rebase, String> {
    let mut work = Work::default();
    for (path, value) in change_list(base, theirs) {
        work.path(base, ours, path, value, merges)?;
    }
    work.removed_under_added(base, ours);
    work.added_under_removed(base, ours, theirs);
    let sets: Vec<Vec<u8>> = work
        .resolved
        .iter()
        .filter(|(_, value)| value.is_some())
        .map(|(path, _)| path.clone())
        .collect();
    let mut checked = BTreeSet::new();
    for path in sets {
        work.check_case(base, ours, theirs, &path, &mut checked);
    }
    let mut state = ours.clone();
    apply_edits(&mut state, &work.resolved);
    work.file_against_directory(&state);

    if work.conflicts.is_empty() {
        return Ok(Rebase::Clean {
            state,
            resolved: work.resolved,
            merged: work.merged,
        });
    }
    let mut conflicts = work.conflicts;
    conflicts.sort_by(|a, b| (&a.path, a.code, &a.detail).cmp(&(&b.path, b.code, &b.detail)));
    conflicts.dedup();
    Ok(Rebase::Conflicts(conflicts))
}

/// The names of one directory of the head, by folded name.
type FoldedNames = BTreeMap<Vec<u8>, Vec<Vec<u8>>>;

/// The state of one rebase.
#[derive(Default)]
struct Work {
    resolved: Edits,
    merged: Vec<Vec<u8>>,
    conflicts: Vec<ModelConflict>,
    /// Paths where the change adds a file and neither the base nor the head
    /// had one.
    added: Vec<Vec<u8>>,
    /// Paths the change deletes.
    deleted: Vec<Vec<u8>>,
    /// Directories of the head whose names were folded already.
    folded: BTreeMap<Vec<u8>, FoldedNames>,
}

impl Work {
    /// The per-path rule for one path the change touches.
    fn path(
        &mut self,
        base: &Files,
        ours: &Files,
        path: Vec<u8>,
        theirs: Option<Value>,
        merges: &mut Vec<MergeCase>,
    ) -> Result<(), String> {
        if theirs.is_none() {
            self.deleted.push(path.clone());
        }
        let b = base.get(&path);
        let o = ours.get(&path);
        if theirs.as_ref() == o {
            return Ok(());
        }
        if b == o {
            if b.is_none() {
                self.added.push(path.clone());
            }
            self.resolved.push((path, theirs));
            return Ok(());
        }
        match (o, theirs) {
            (Some(o), Some(t)) => self.merge(path, b, o, &t, merges)?,
            (None, Some(_)) => {
                let conflict = match find(ours, &path) {
                    Found::Dir => ModelConflict::new(&path, "DF"),
                    Found::Blocked(len) => ModelConflict::new(&path[..len], "DF"),
                    _ => ModelConflict::new(&path, "MODIFY_DELETE"),
                };
                self.conflicts.push(conflict);
            }
            _ => self
                .conflicts
                .push(ModelConflict::new(&path, "MODIFY_DELETE")),
        }
        Ok(())
    }

    /// Both sides changed the file at `path` to different files.
    fn merge(
        &mut self,
        path: Vec<u8>,
        base: Option<&Value>,
        ours: &Value,
        theirs: &Value,
        merges: &mut Vec<MergeCase>,
    ) -> Result<(), String> {
        if !ours.is_regular() || !theirs.is_regular() {
            let code = if ours.mode() == theirs.mode() {
                "CONTENT"
            } else {
                "MODE"
            };
            self.conflicts.push(ModelConflict::new(&path, code));
            return Ok(());
        }
        let base = base.filter(|value| value.is_regular());
        let (o, t) = (ours.mode() == Mode::Exec, theirs.mode() == Mode::Exec);
        // A side that kept the base's bit yields to the other.
        let exec = if o == t {
            Some(o)
        } else {
            base.map(|base| {
                if (base.mode() == Mode::Exec) == o {
                    t
                } else {
                    o
                }
            })
        };
        let content = if ours.content() == theirs.content() {
            TextMerge::Clean(ours.content().to_vec())
        } else {
            let base_text = base.map_or(&[][..], Value::content);
            let result = merge_text(base_text, ours.content(), theirs.content())
                .map_err(|why| format!("cannot merge {}: {why}", show(&path)))?;
            merges.push(MergeCase {
                path: path.clone(),
                base: base_text.to_vec(),
                ours: ours.content().to_vec(),
                theirs: theirs.content().to_vec(),
                result: result.clone(),
            });
            result
        };
        if exec.is_none() {
            self.conflicts.push(ModelConflict::new(&path, "MODE"));
        }
        match (exec, content) {
            (_, TextMerge::Conflict(hunks)) => self.conflicts.push(ModelConflict {
                hunks,
                ..ModelConflict::new(&path, "CONTENT")
            }),
            (Some(exec), TextMerge::Clean(text)) => {
                let mode = if exec { Mode::Exec } else { Mode::File };
                let value = Value::new(mode, text);
                if value != *ours {
                    self.resolved.push((path.clone(), Some(value)));
                    self.merged.push(path);
                }
            }
            (None, TextMerge::Clean(_)) => {}
        }
        Ok(())
    }

    /// A file the change adds under a directory of the base that the head
    /// removed.
    fn removed_under_added(&mut self, base: &Files, ours: &Files) {
        for path in &self.added {
            let deepest = prefix_lengths(path)
                .filter(|&len| is_dir(base, &path[..len]))
                .last();
            if let Some(len) = deepest
                && find(ours, &path[..len]) == Found::Absent
            {
                self.conflicts.push(ModelConflict {
                    detail: path[..len].to_vec(),
                    ..ModelConflict::new(path, "DIR_REMOVED")
                });
            }
        }
    }

    /// A file the head added under a directory of the base that the change
    /// removed.
    fn added_under_removed(&mut self, base: &Files, ours: &Files, theirs: &Files) {
        let removed: BTreeSet<Vec<u8>> = self
            .deleted
            .iter()
            .filter_map(|path| first_missing_dir(theirs, path).map(|len| path[..len].to_vec()))
            .collect();
        for dir in removed {
            if find(base, &dir) != Found::Dir || find(ours, &dir) != Found::Dir {
                continue;
            }
            for (path, _) in files_below(ours, &dir) {
                if !base.contains_key(path) {
                    self.conflicts.push(ModelConflict {
                        detail: dir.clone(),
                        ..ModelConflict::new(path, "DIR_REMOVED")
                    });
                }
            }
        }
    }

    /// The change sets `path`: the first name on it that the head does not
    /// have must not fold to a name the head added.
    fn check_case(
        &mut self,
        base: &Files,
        ours: &Files,
        theirs: &Files,
        path: &[u8],
        checked: &mut BTreeSet<Vec<u8>>,
    ) {
        let mut start = 0;
        loop {
            let end = path[start..]
                .iter()
                .position(|byte| *byte == b'/')
                .map_or(path.len(), |at| start + at);
            let prefix = &path[..end];
            let last = end == path.len();
            if ours.contains_key(prefix) {
                return;
            }
            if is_dir(ours, prefix) {
                if last {
                    return;
                }
                start = end + 1;
                continue;
            }
            if !checked.insert(prefix.to_vec()) {
                return;
            }
            let parent = &path[..start.saturating_sub(1)];
            let index = self.folded.entry(parent.to_vec()).or_insert_with(|| {
                let mut index = FoldedNames::new();
                for name in names_in(ours, parent) {
                    index.entry(fold(&name)).or_default().push(name);
                }
                index
            });
            let others = index
                .get(&fold(&path[start..end]))
                .cloned()
                .unwrap_or_default();
            for other in others {
                let existing = join(parent, &other);
                // Only a name the head brought counts.
                if !has_entry(base, &existing) && !has_entry(theirs, &existing) {
                    self.conflicts.push(ModelConflict {
                        detail: existing,
                        ..ModelConflict::new(prefix, "CASE")
                    });
                }
            }
            return;
        }
    }

    /// In the result, a file where another file needs a directory.
    fn file_against_directory(&mut self, state: &Files) {
        let mut clashes = BTreeSet::new();
        for path in state.keys() {
            for len in prefix_lengths(path) {
                if state.contains_key(&path[..len]) {
                    clashes.insert(path[..len].to_vec());
                }
            }
        }
        for path in clashes {
            self.conflicts.push(ModelConflict::new(&path, "DF"));
        }
    }
}

/// Builds tree ids from scratch from a state, with the format crate's
/// `build_directory`, remembering directories it has built already.
#[derive(Default)]
pub struct Oracle {
    built: HashMap<Vec<Entry>, ObjectId>,
}

impl fmt::Debug for Oracle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Oracle")
            .field("directories", &self.built.len())
            .finish()
    }
}

/// How many directories the oracle remembers before it starts again.
const ORACLE_MEMORY: usize = 20_000;

impl Oracle {
    /// The root tree id of `files`.
    pub fn root(&mut self, files: &Files) -> ObjectId {
        let items: Vec<(&[u8], &Value)> = files
            .iter()
            .map(|(path, value)| (path.as_slice(), value))
            .collect();
        self.build(&items)
    }

    /// The tree id of the directory `dir`, if there is one.
    pub fn dir(&mut self, files: &Files, dir: &[u8]) -> Option<ObjectId> {
        let skip = dir.len() + 1;
        let items: Vec<(&[u8], &Value)> = files_below(files, dir)
            .map(|(path, value)| (&path[skip..], value))
            .collect();
        (!items.is_empty()).then(|| self.build(&items))
    }

    /// The entry at `path`, as the engine would report it in a conflict.
    pub fn entry_at(&mut self, files: &Files, path: &[u8]) -> Option<EntryKind> {
        match find(files, path) {
            Found::File(value) => Some(value.entry()),
            Found::Dir => self.dir(files, path).map(|tree| EntryKind::Tree { tree }),
            Found::Absent | Found::Blocked(_) => None,
        }
    }

    fn build(&mut self, items: &[(&[u8], &Value)]) -> ObjectId {
        let mut entries = Vec::new();
        let mut below: BTreeMap<&[u8], Vec<(&[u8], &Value)>> = BTreeMap::new();
        for (path, value) in items {
            match path.iter().position(|byte| *byte == b'/') {
                None => entries.push(Entry {
                    name: path.to_vec(),
                    kind: value.entry(),
                }),
                Some(slash) => below
                    .entry(&path[..slash])
                    .or_default()
                    .push((&path[slash + 1..], *value)),
            }
        }
        for (name, sub) in below {
            let tree = self.build(&sub);
            entries.push(Entry {
                name: name.to_vec(),
                kind: EntryKind::Tree { tree },
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        if let Some(id) = self.built.get(&entries) {
            return *id;
        }
        let id = build_directory(entries.clone())
            .expect("a state always forms valid directories")
            .root();
        if self.built.len() >= ORACLE_MEMORY {
            self.built.clear();
        }
        self.built.insert(entries, id);
        id
    }
}

/// A path for messages: quoted, with combining marks escaped so that the
/// two forms of a name tell apart.
pub fn show(path: &[u8]) -> String {
    format!("{:?}", String::from_utf8_lossy(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[&str]) -> Vec<u8> {
        lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>()
            .into_bytes()
    }

    fn clean(base: &[&str], ours: &[&str], theirs: &[&str]) -> Vec<u8> {
        match merge_text(&text(base), &text(ours), &text(theirs)).unwrap() {
            TextMerge::Clean(bytes) => bytes,
            conflict => panic!("expected a clean merge, got {conflict:?}"),
        }
    }

    fn conflict(base: &[&str], ours: &[&str], theirs: &[&str]) -> Vec<Hunk> {
        match merge_text(&text(base), &text(ours), &text(theirs)).unwrap() {
            TextMerge::Conflict(hunks) => hunks,
            clean => panic!("expected a conflict, got {clean:?}"),
        }
    }

    #[test]
    fn line_merge_follows_the_rules() {
        let base = ["1", "2", "3", "4", "5"];
        // Separate edits, one unchanged line between them.
        assert_eq!(
            clean(
                &base,
                &["1", "A", "3", "4", "5"],
                &["1", "2", "3", "B", "5"]
            ),
            text(&["1", "A", "3", "B", "5"])
        );
        // Adjacent edits touch.
        let hunks = conflict(
            &base,
            &["1", "A", "3", "4", "5"],
            &["1", "2", "B", "4", "5"],
        );
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].base, (1..3, text(&["2", "3"])));
        assert_eq!(hunks[0].ours, (1..3, text(&["A", "3"])));
        assert_eq!(hunks[0].theirs, (1..3, text(&["2", "B"])));
        // An insertion at the edge of a change touches it.
        conflict(
            &base,
            &["1", "A", "3", "4", "5"],
            &["1", "2", "X", "3", "4", "5"],
        );
        // An insertion one line away does not.
        assert_eq!(
            clean(
                &base,
                &["1", "A", "3", "4", "5"],
                &["1", "2", "3", "X", "4", "5"]
            ),
            text(&["1", "A", "3", "X", "4", "5"])
        );
        // The same change on both sides is taken once.
        assert_eq!(
            clean(
                &base,
                &["1", "A", "3", "4", "5"],
                &["1", "A", "3", "4", "Z"]
            ),
            text(&["1", "A", "3", "4", "Z"])
        );
        // Insertions at the same place.
        conflict(&["a", "b"], &["a", "X", "b"], &["a", "Y", "b"]);
        // Line numbers on each side follow what came before.
        let base = ["1", "2", "3", "4", "5", "6"];
        let hunks = conflict(
            &base,
            &["1", "2", "n", "n2", "3", "4", "X", "6"],
            &["2", "3", "4", "Y", "6"],
        );
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].base, (4..5, text(&["5"])));
        assert_eq!(hunks[0].ours, (6..7, text(&["X"])));
        assert_eq!(hunks[0].theirs, (3..4, text(&["Y"])));
        // An insertion right after a line the other side deletes touches it.
        conflict(
            &base,
            &["1", "n", "2", "3", "4", "5", "6"],
            &["2", "3", "4", "5", "6"],
        );
        // Binary content.
        assert_eq!(
            merge_text(b"\0a", b"\0b", b"\0c").unwrap(),
            TextMerge::Conflict(Vec::new())
        );
        // What the model cannot judge.
        assert!(merge_text(&text(&["a", "b"]), &text(&["b", "a", "c"]), &text(&["a"])).is_err());
        assert!(merge_text(&text(&["a"]), &text(&["a", "a"]), &text(&["b"])).is_err());
    }

    fn files(entries: &[(&str, &str)]) -> Files {
        entries
            .iter()
            .map(|(path, content)| {
                (
                    path.as_bytes().to_vec(),
                    Value::new(Mode::File, content.as_bytes().to_vec()),
                )
            })
            .collect()
    }

    fn codes(result: Rebase) -> Vec<(String, &'static str)> {
        match result {
            Rebase::Conflicts(conflicts) => conflicts
                .into_iter()
                .map(|c| (String::from_utf8(c.path).unwrap(), c.code))
                .collect(),
            Rebase::Clean { state, .. } => panic!("expected conflicts, got {state:?}"),
        }
    }

    #[test]
    fn rebase_rules() {
        let base = files(&[("a", "1\n"), ("d/x", "1\n"), ("k", "1\n")]);
        let mut merges = Vec::new();
        // Disjoint edits.
        let ours = files(&[("a", "2\n"), ("d/x", "1\n"), ("k", "1\n")]);
        let theirs = files(&[("a", "1\n"), ("d/x", "1\n"), ("d/y", "1\n"), ("k", "1\n")]);
        match rebase(&base, &ours, &theirs, &mut merges).unwrap() {
            Rebase::Clean { state, .. } => assert_eq!(
                state,
                files(&[("a", "2\n"), ("d/x", "1\n"), ("d/y", "1\n"), ("k", "1\n")])
            ),
            other => panic!("{other:?}"),
        }
        // The head removed `d`; the change adds under it.
        let ours = files(&[("a", "1\n"), ("k", "1\n")]);
        assert_eq!(
            codes(rebase(&base, &ours, &theirs, &mut merges).unwrap()),
            [("d/y".to_owned(), "DIR_REMOVED")]
        );
        // A file against a directory, and a name twin.
        let ours = files(&[
            ("a", "1\n"),
            ("d/x", "1\n"),
            ("k", "1\n"),
            ("README", "h\n"),
        ]);
        let theirs = files(&[
            ("a/b", "1\n"),
            ("d/x", "1\n"),
            ("k", "1\n"),
            ("readme", "c\n"),
        ]);
        let ours_a_file = {
            let mut ours = ours.clone();
            ours.insert(b"a".to_vec(), Value::new(Mode::File, b"2\n".to_vec()));
            ours
        };
        // The change deleted `a`, which the head modified, and put `a/b` below
        // it: a modify/delete conflict, and the file the head keeps at `a`
        // stands where `a/b` needs a directory.
        assert_eq!(
            codes(rebase(&base, &ours_a_file, &theirs, &mut merges).unwrap()),
            [
                ("a".to_owned(), "DF"),
                ("a".to_owned(), "MODIFY_DELETE"),
                ("readme".to_owned(), "CASE")
            ]
        );
    }

    #[test]
    fn oracle_matches_the_empty_tree_and_shares_directories() {
        let mut oracle = Oracle::default();
        assert_eq!(
            oracle.root(&Files::new()).to_hex(),
            "3136b0f18562ee73b840ea348c54ae3c7e97e4cc3e547ea9dd03fa1cd8b384cf"
        );
        let state = files(&[("d/x", "1\n"), ("e/x", "1\n")]);
        assert_eq!(oracle.dir(&state, b"d"), oracle.dir(&state, b"e"));
        assert_eq!(oracle.dir(&state, b"f"), None);
    }

    #[test]
    fn folding() {
        assert_eq!(fold(b"README"), fold(b"readme"));
        assert_eq!(fold("caf\u{e9}".as_bytes()), fold("cafe\u{301}".as_bytes()));
        assert_eq!(fold("CAF\u{c9}".as_bytes()), fold("cafe\u{301}".as_bytes()));
        assert_ne!(fold(b"a"), fold(b"b"));
        assert_eq!(fold(b"\xffA"), b"\xffa");
    }
}
