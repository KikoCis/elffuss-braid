// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The edits agents make: small random changes to a state, kept as
//! operations so that an agent can redo them on a newer head after a
//! conflict.
//!
//! Paths come from a small set of names, so agents often touch the same files
//! and directories. Some names differ only in letter case or Unicode
//! normalisation, and some are used both as file and directory names. Every
//! line of text is new when it is written, so no line appears twice in a
//! version of a file and lines never move, which keeps the model's line
//! merge exact.

use std::collections::{BTreeMap, BTreeSet};

use braid_format::tree::MAX_FLAT_ENTRIES;

use crate::model::{Files, Mode, Value, files_below, inside, prefix_lengths};
use crate::rng::Rng;

/// Directory names.
const DIRS: &[&str] = &[
    "src",
    "docs",
    "Docs",
    "lib",
    "a",
    "b",
    "caf\u{e9}",
    "cafe\u{301}",
];

/// File names.
const LEAVES: &[&str] = &[
    "main.rs",
    "lib.rs",
    "README",
    "readme",
    "ReadMe",
    "notes",
    "x",
    "y",
    "a",
    "b",
    "caf\u{e9}",
    "cafe\u{301}",
    "Makefile",
];

/// Files that many edits go to.
const HOT: &[&str] = &["src/main.rs", "docs/README", "notes"];

/// The directory that holds about a thousand files, on either side of the
/// largest flat directory, in the runs that have it.
pub const WIDE: &[u8] = b"w";

/// The most entries a directory stored as one flat node has.
pub const FLAT_LIMIT: usize = MAX_FLAT_ENTRIES;

/// One edit.
#[derive(Clone, Debug)]
pub enum Op {
    /// Sets a file, deleting whatever is in the way: a file at a prefix of
    /// the path, or the files below it.
    Write {
        /// Where.
        path: Vec<u8>,
        /// The file.
        value: Value,
    },
    /// Edits lines of a text file.
    Edit {
        /// The file.
        path: Vec<u8>,
        /// The line edits, in order.
        edits: Vec<LineEdit>,
    },
    /// Deletes a file.
    Delete {
        /// The file.
        path: Vec<u8>,
    },
    /// Deletes every file below a directory.
    DeleteDir {
        /// The directory.
        dir: Vec<u8>,
    },
    /// Changes the mode of a regular file.
    SetMode {
        /// The file.
        path: Vec<u8>,
        /// Its new mode.
        mode: Mode,
    },
}

/// One edit of the lines of a text, by line content rather than position, so
/// that it still means something on another version of the text.
#[derive(Clone, Debug)]
pub enum LineEdit {
    /// Replaces a line.
    Replace {
        /// The line replaced.
        old: Vec<u8>,
        /// The new line.
        new: Vec<u8>,
    },
    /// Inserts a line after another, or first.
    Insert {
        /// The line before it; `None` for the first line.
        after: Option<Vec<u8>>,
        /// The new line.
        new: Vec<u8>,
    },
    /// Removes a line.
    Remove {
        /// The line removed.
        old: Vec<u8>,
    },
}

/// Applies an edit. An edit that no longer fits the state, such as changing
/// lines of a file that is gone, does nothing.
pub fn apply(files: &mut Files, op: &Op) {
    match op {
        Op::Write { path, value } => write(files, path, value.clone()),
        Op::Edit { path, edits } => {
            if let Some(value) = files.get(path)
                && value.is_text()
            {
                let content = edit_lines(value.content(), edits);
                if content != value.content() {
                    let edited = Value::new(value.mode(), content);
                    files.insert(path.clone(), edited);
                }
            }
        }
        Op::Delete { path } => {
            files.remove(path);
        }
        Op::DeleteDir { dir } => {
            let doomed: Vec<Vec<u8>> = files_below(files, dir).map(|(p, _)| p.clone()).collect();
            for path in doomed {
                files.remove(&path);
            }
        }
        Op::SetMode { path, mode } => {
            if let Some(value) = files.get(path)
                && value.is_regular()
                && value.mode() != *mode
            {
                let changed = value.with_mode(*mode);
                files.insert(path.clone(), changed);
            }
        }
    }
}

/// Sets `path`, after deleting a file at one of its prefixes and the files
/// below it.
fn write(files: &mut Files, path: &[u8], value: Value) {
    for len in prefix_lengths(path) {
        files.remove(&path[..len]);
    }
    let doomed: Vec<Vec<u8>> = files_below(files, path).map(|(p, _)| p.clone()).collect();
    for below in doomed {
        files.remove(&below);
    }
    files.insert(path.to_vec(), value);
}

fn split_lines(content: &[u8]) -> Vec<Vec<u8>> {
    if content.is_empty() {
        return Vec::new();
    }
    content
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

/// Applies line edits. A new line already in the text is not added again,
/// and an edit whose line is gone does nothing.
fn edit_lines(content: &[u8], edits: &[LineEdit]) -> Vec<u8> {
    let mut lines = split_lines(content);
    for edit in edits {
        match edit {
            LineEdit::Replace { old, new } => {
                if !lines.contains(new)
                    && let Some(at) = lines.iter().position(|line| line == old)
                {
                    lines[at] = new.clone();
                }
            }
            LineEdit::Insert { after, new } => {
                if !lines.contains(new) {
                    let at = match after {
                        None => 0,
                        Some(after) => lines
                            .iter()
                            .position(|line| line == after)
                            .map_or(lines.len(), |at| at + 1),
                    };
                    lines.insert(at, new.clone());
                }
            }
            LineEdit::Remove { old } => {
                if let Some(at) = lines.iter().position(|line| line == old) {
                    lines.remove(at);
                }
            }
        }
    }
    lines.concat()
}

/// Makes random edits, with lines and names that are new each time.
#[derive(Debug, Default)]
pub struct Generator {
    fresh: u64,
}

impl Generator {
    fn next(&mut self) -> u64 {
        self.fresh += 1;
        self.fresh
    }

    /// A line no text has had before.
    pub fn line(&mut self) -> Vec<u8> {
        format!("line {}\n", self.next()).into_bytes()
    }

    /// `count` new lines.
    pub fn text(&mut self, count: usize) -> Vec<u8> {
        (0..count).flat_map(|_| self.line()).collect()
    }

    fn wide_name(&mut self) -> Vec<u8> {
        // Long names, so that rewriting the whole directory needs a change
        // list too long to be inline.
        let mut path = WIDE.to_vec();
        path.extend_from_slice(format!("/entry-{:06}-with-a-longer-name", self.next()).as_bytes());
        path
    }

    /// The state a run starts from, with a directory of about a thousand
    /// files if `wide`.
    pub fn initial(&mut self, rng: &mut Rng, wide: bool) -> Files {
        let mut files = Files::new();
        // The files most edits go to are long enough for edits of different
        // agents to fall apart and merge, or to touch and conflict.
        let texts: [(&str, usize); 7] = [
            ("src/main.rs", 30),
            ("src/lib.rs", 12),
            ("docs/README", 20),
            ("notes", 20),
            ("a/x", 4),
            ("b/y", 4),
            ("lib/util.rs", 6),
        ];
        for (path, lines) in texts {
            let value = Value::new(Mode::File, self.text(lines));
            files.insert(path.as_bytes().to_vec(), value);
        }
        files.insert(b"run.sh".to_vec(), Value::new(Mode::Exec, self.text(3)));
        files.insert(b"link".to_vec(), Value::new(Mode::Link, b"notes".to_vec()));
        files.insert(
            b"logo.bin".to_vec(),
            Value::new(Mode::File, b"\0\x01binary".to_vec()),
        );
        if wide {
            for _ in 0..rng.range(FLAT_LIMIT - 24, FLAT_LIMIT + 25) {
                let path = self.wide_name();
                files.insert(path, Value::new(Mode::File, self.line()));
            }
        }
        files
    }

    /// A random small change to `files`: the edits, and the state they give.
    pub fn change(&mut self, rng: &mut Rng, files: &Files) -> (Vec<Op>, Files) {
        let mut state = files.clone();
        let mut ops = Vec::new();
        let count = match rng.below(20) {
            0..=9 => 1,
            10..=15 => 2,
            16..=18 => 3,
            _ => 5,
        };
        for _ in 0..count {
            for op in self.one(rng, &state) {
                apply(&mut state, &op);
                ops.push(op);
            }
        }
        (ops, state)
    }

    /// The same edits with new lines, to redo them on a newer head. A line
    /// that a later edit refers to is renamed there too.
    pub fn refresh(&mut self, ops: &[Op]) -> Vec<Op> {
        let mut renamed: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut fresh = |line: &Vec<u8>, renamed: &mut BTreeMap<Vec<u8>, Vec<u8>>| {
            let new = self.line();
            renamed.insert(line.clone(), new.clone());
            new
        };
        let rename = |line: &Vec<u8>, renamed: &BTreeMap<Vec<u8>, Vec<u8>>| {
            renamed.get(line).cloned().unwrap_or_else(|| line.clone())
        };
        ops.iter()
            .map(|op| match op {
                Op::Edit { path, edits } => Op::Edit {
                    path: path.clone(),
                    edits: edits
                        .iter()
                        .map(|edit| match edit {
                            LineEdit::Replace { old, new } => LineEdit::Replace {
                                old: rename(old, &renamed),
                                new: fresh(new, &mut renamed),
                            },
                            LineEdit::Insert { after, new } => LineEdit::Insert {
                                after: after.as_ref().map(|after| rename(after, &renamed)),
                                new: fresh(new, &mut renamed),
                            },
                            LineEdit::Remove { old } => LineEdit::Remove {
                                old: rename(old, &renamed),
                            },
                        })
                        .collect(),
                },
                other => other.clone(),
            })
            .collect()
    }

    /// One random edit; several for edits of the wide directory.
    fn one(&mut self, rng: &mut Rng, files: &Files) -> Vec<Op> {
        let ordinary: Vec<&Vec<u8>> = files.keys().filter(|path| !inside(WIDE, path)).collect();
        let pick = |rng: &mut Rng, paths: &[&Vec<u8>]| -> Option<Vec<u8>> {
            (!paths.is_empty()).then(|| (*rng.pick(paths)).clone())
        };
        match rng.below(100) {
            0..=34 => {
                let hot: Vec<&Vec<u8>> = ordinary
                    .iter()
                    .copied()
                    .filter(|path| HOT.iter().any(|hot| hot.as_bytes() == path.as_slice()))
                    .filter(|path| files[*path].is_text())
                    .collect();
                let texts: Vec<&Vec<u8>> = ordinary
                    .iter()
                    .copied()
                    .filter(|path| files[*path].is_text())
                    .collect();
                let from = if !hot.is_empty() && rng.chance(0.5) {
                    &hot
                } else {
                    &texts
                };
                if let Some(path) = pick(rng, from) {
                    let edits = self.line_edits(rng, &files[&path]);
                    return vec![Op::Edit { path, edits }];
                }
            }
            35..=46 => {}
            47..=51 => {
                if let Some(path) = pick(rng, &ordinary) {
                    let value = self.file(rng);
                    return vec![Op::Write { path, value }];
                }
            }
            52..=60 => {
                if let Some(path) = pick(rng, &ordinary) {
                    return vec![Op::Delete { path }];
                }
            }
            61..=64 => {
                let dirs = directories(&ordinary);
                if !dirs.is_empty() {
                    let dir = rng.pick(&dirs).clone();
                    return vec![Op::DeleteDir { dir }];
                }
            }
            65..=69 => {
                let regular: Vec<&Vec<u8>> = ordinary
                    .iter()
                    .copied()
                    .filter(|path| files[*path].is_regular())
                    .collect();
                if let Some(path) = pick(rng, &regular) {
                    let mode = match files[&path].mode() {
                        Mode::Exec => Mode::File,
                        _ => Mode::Exec,
                    };
                    return vec![Op::SetMode { path, mode }];
                }
            }
            70..=71 => {
                let target = format!("target-{}", self.next()).into_bytes();
                let path = self.path(rng);
                return vec![Op::Write {
                    path,
                    value: Value::new(Mode::Link, target),
                }];
            }
            72..=76 => {
                if let Some(path) = pick(rng, &ordinary)
                    && let Some(twin) = self.twin_path(rng, &path)
                {
                    let value = self.file(rng);
                    return vec![Op::Write { path: twin, value }];
                }
            }
            77..=80 => {
                // A file where a directory is, or a directory where a file is.
                let dirs = directories(&ordinary);
                if !dirs.is_empty() && rng.chance(0.5) {
                    let path = rng.pick(&dirs).clone();
                    let value = self.file(rng);
                    return vec![Op::Write { path, value }];
                }
                if let Some(mut path) = pick(rng, &ordinary) {
                    path.push(b'/');
                    path.extend_from_slice(rng.pick(LEAVES).as_bytes());
                    let value = self.file(rng);
                    return vec![Op::Write { path, value }];
                }
            }
            81..=84 => {
                let ops = self.wide(rng, files);
                if !ops.is_empty() {
                    return ops;
                }
            }
            85..=97 => {}
            _ => {
                let path = self.path(rng);
                let content = format!("\0binary {}", self.next()).into_bytes();
                return vec![Op::Write {
                    path,
                    value: Value::new(Mode::File, content),
                }];
            }
        }
        let path = self.path(rng);
        let value = self.file(rng);
        vec![Op::Write { path, value }]
    }

    /// A random path of one to three names.
    fn path(&mut self, rng: &mut Rng) -> Vec<u8> {
        let depth = match rng.below(20) {
            0..=6 => 1,
            7..=15 => 2,
            _ => 3,
        };
        let mut names: Vec<Vec<u8>> = (1..depth)
            .map(|_| rng.pick(DIRS).as_bytes().to_vec())
            .collect();
        if rng.chance(0.25) {
            names.push(format!("f{}", self.next()).into_bytes());
        } else {
            names.push(rng.pick(LEAVES).as_bytes().to_vec());
        }
        names.join(&b'/')
    }

    /// A random new file: mostly text, sometimes executable, now and then
    /// empty.
    fn file(&mut self, rng: &mut Rng) -> Value {
        match rng.below(20) {
            0..=1 => Value::new(Mode::Exec, self.text(rng.range(1, 5))),
            2 => Value::new(Mode::File, Vec::new()),
            _ => Value::new(Mode::File, self.text(rng.range(1, 9))),
        }
    }

    /// One to three edits of the lines of a text file.
    fn line_edits(&mut self, rng: &mut Rng, value: &Value) -> Vec<LineEdit> {
        let mut lines = split_lines(value.content());
        let mut edits = Vec::new();
        for _ in 0..rng.range(1, 4) {
            let edit = if lines.is_empty() {
                LineEdit::Insert {
                    after: None,
                    new: self.line(),
                }
            } else {
                match rng.below(3) {
                    0 => LineEdit::Replace {
                        old: rng.pick(&lines).clone(),
                        new: self.line(),
                    },
                    1 if rng.chance(0.15) => LineEdit::Insert {
                        after: None,
                        new: self.line(),
                    },
                    1 => LineEdit::Insert {
                        after: Some(rng.pick(&lines).clone()),
                        new: self.line(),
                    },
                    _ => LineEdit::Remove {
                        old: rng.pick(&lines).clone(),
                    },
                }
            };
            lines = split_lines(&edit_lines(&lines.concat(), std::slice::from_ref(&edit)));
            edits.push(edit);
        }
        edits
    }

    /// A path next to `path` whose name at some level differs from `path`'s
    /// only in case or normalisation.
    fn twin_path(&mut self, rng: &mut Rng, path: &[u8]) -> Option<Vec<u8>> {
        let names: Vec<&[u8]> = path.split(|byte| *byte == b'/').collect();
        let level = rng.below(names.len());
        let twin = twin_name(rng, names[level])?;
        let mut out: Vec<Vec<u8>> = names[..level].iter().map(|name| name.to_vec()).collect();
        out.push(twin);
        if level + 1 < names.len() {
            out.push(rng.pick(LEAVES).as_bytes().to_vec());
        }
        Some(out.join(&b'/'))
    }

    /// Adds or deletes files in the wide directory, mostly toward the other
    /// side of the flat limit; now and then rewrites all of them.
    fn wide(&mut self, rng: &mut Rng, files: &Files) -> Vec<Op> {
        let entries: Vec<Vec<u8>> = files_below(files, WIDE).map(|(p, _)| p.clone()).collect();
        if entries.is_empty() {
            return Vec::new();
        }
        if rng.chance(0.02) {
            return entries
                .into_iter()
                .map(|path| Op::Write {
                    path,
                    value: Value::new(Mode::File, self.line()),
                })
                .collect();
        }
        let count = rng.range(1, 41);
        let add = if entries.len() <= FLAT_LIMIT {
            rng.chance(0.75)
        } else {
            rng.chance(0.25)
        };
        if add {
            return (0..count)
                .map(|_| Op::Write {
                    path: self.wide_name(),
                    value: Value::new(Mode::File, self.line()),
                })
                .collect();
        }
        let mut doomed = BTreeSet::new();
        for _ in 0..count {
            doomed.insert(rng.pick(&entries).clone());
        }
        doomed.into_iter().map(|path| Op::Delete { path }).collect()
    }
}

/// The directories that hold the given files.
fn directories(paths: &[&Vec<u8>]) -> Vec<Vec<u8>> {
    let dirs: BTreeSet<Vec<u8>> = paths
        .iter()
        .flat_map(|path| prefix_lengths(path).map(|len| path[..len].to_vec()))
        .collect();
    dirs.into_iter().collect()
}

/// Another name that folds to the same name: other letter case, or the other
/// normalisation of `café`.
fn twin_name(rng: &mut Rng, name: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(name).ok()?;
    let mut twins: Vec<String> = Vec::new();
    match text {
        "caf\u{e9}" => twins.extend(["cafe\u{301}".to_owned(), "CAF\u{c9}".to_owned()]),
        "cafe\u{301}" => twins.extend(["caf\u{e9}".to_owned(), "Cafe\u{301}".to_owned()]),
        _ => {
            twins.push(text.to_ascii_uppercase());
            twins.push(text.to_ascii_lowercase());
            let mut chars = text.chars();
            if let Some(first) = chars.next() {
                twins.push(first.to_ascii_uppercase().to_string() + chars.as_str());
            }
        }
    }
    twins.retain(|twin| twin != text);
    twins.dedup();
    (!twins.is_empty()).then(|| rng.pick(&twins).clone().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{fold, merge_text};

    #[test]
    fn writes_clear_the_way() {
        let mut files = Files::new();
        let value = Value::new(Mode::File, b"1\n".to_vec());
        apply(
            &mut files,
            &Op::Write {
                path: b"a/b".to_vec(),
                value: value.clone(),
            },
        );
        apply(
            &mut files,
            &Op::Write {
                path: b"a/b/c".to_vec(),
                value: value.clone(),
            },
        );
        assert_eq!(files.keys().collect::<Vec<_>>(), [b"a/b/c"]);
        apply(
            &mut files,
            &Op::Write {
                path: b"a".to_vec(),
                value,
            },
        );
        assert_eq!(files.keys().collect::<Vec<_>>(), [b"a"]);
    }

    #[test]
    fn line_edits_never_repeat_a_line() {
        let edits = [
            LineEdit::Insert {
                after: None,
                new: b"x\n".to_vec(),
            },
            LineEdit::Insert {
                after: Some(b"b\n".to_vec()),
                new: b"x\n".to_vec(),
            },
            LineEdit::Replace {
                old: b"a\n".to_vec(),
                new: b"b\n".to_vec(),
            },
            LineEdit::Remove {
                old: b"gone\n".to_vec(),
            },
        ];
        assert_eq!(edit_lines(b"a\nb\n", &edits), b"x\na\nb\n");
    }

    #[test]
    fn twins_fold_alike() {
        let mut rng = Rng::new(3);
        for name in ["README", "caf\u{e9}", "cafe\u{301}", "src", "a"] {
            let twin = twin_name(&mut rng, name.as_bytes()).unwrap();
            assert_ne!(twin, name.as_bytes());
            assert_eq!(fold(&twin), fold(name.as_bytes()), "{name}");
        }
        assert_eq!(twin_name(&mut rng, b"123"), None);
    }

    #[test]
    fn random_changes_keep_lines_unique_and_in_order() {
        let mut rng = Rng::new(11);
        let mut generator = Generator::default();
        let start = generator.initial(&mut rng, false);
        let mut state = start.clone();
        for _ in 0..300 {
            let (ops, next) = generator.change(&mut rng, &state);
            // A redo on the old state, with new lines, is just as valid.
            let mut redone = state.clone();
            for op in &generator.refresh(&ops) {
                apply(&mut redone, op);
            }
            for version in [&next, &redone] {
                for (path, value) in version {
                    if let Some(before) = start.get(path)
                        && before.is_text()
                        && value.is_text()
                    {
                        // A third version that differs from both, so that
                        // the merge has to diff them.
                        let other = [before.content(), b"unrelated\n"].concat();
                        merge_text(before.content(), value.content(), &other).unwrap();
                    }
                }
            }
            state = next;
        }
    }
}
