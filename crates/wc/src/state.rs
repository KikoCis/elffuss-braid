// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The state of a workspace, kept in the repository under
//! `.braid/ws/<name>/`:
//!
//! ```text
//! state    what the workspace is at (below)
//! stat     the stat cache
//! lock     locked while a command changes the workspace
//! ```
//!
//! `state` is a small text file, rewritten whole through a scratch file and
//! a rename. Values are escaped with [`braid_repo::text::escape`].
//!
//! ```text
//! braid-workspace-state 1
//! path <workspace directory>
//! branch <branch it lands on>
//! base <changeset the working copy is at, or none>
//! upstream <last changeset of the branch it was brought onto, or none>
//! fs ignores-case=<yes|no> ignores-normalization=<yes|no>
//! checkout <from tree> <to tree>           only while a checkout is unfinished
//! track <path>                             one per `braid track`
//! rebase-change <changeset>                only while a rebase is in progress:
//! rebase-onto <changeset>                    the changeset being rebased, where
//! rebase-pending <changeset>                 to, those still to rebase after it,
//! conflict <code> <markers> <resolved> <path>   and its conflicts
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use braid_format::ObjectId;
use braid_repo::text::{escape, path_bytes, path_from_bytes, unescape};

use crate::error::Error;
use crate::fsprobe::{FileSystem, random_hex};

const HEADER: &str = "braid-workspace-state 1";

/// The file that holds the state, inside the workspace's state directory.
pub(crate) const FILE: &str = "state";

/// The lock file, inside the workspace's state directory.
pub(crate) const LOCK: &str = "lock";

/// One conflict of a rebase in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedConflict {
    /// Where it is.
    pub path: Vec<u8>,
    /// Its kind, as [`braid_engine::ConflictKind::code`] names it.
    pub code: String,
    /// Whether the file was written with conflict markers. Such a conflict
    /// is resolved by editing them out; any other by `braid resolve`.
    pub markers: bool,
    /// Whether `braid resolve` marked it resolved.
    pub resolved: bool,
}

/// A checkout that was started and not finished: the working copy may be
/// anywhere between the two trees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkout {
    /// The tree the working copy was at.
    pub from: ObjectId,
    /// The tree it was being brought to.
    pub to: ObjectId,
}

/// A rebase stopped by conflicts, waiting for `braid commit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rebase {
    /// The changeset being rebased.
    pub change: ObjectId,
    /// Where it goes: the workspace's base while the rebase lasts, and the
    /// parent of the changeset that `braid commit` makes.
    pub onto: ObjectId,
    /// Changesets to rebase after it, oldest first.
    pub pending: Vec<ObjectId>,
    /// Its conflicts.
    pub conflicts: Vec<RecordedConflict>,
}

/// What a workspace is at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    /// The workspace directory.
    pub path: PathBuf,
    /// The branch it lands on and updates from.
    pub branch: String,
    /// The changeset its working copy is at; `None` before the first commit
    /// on a branch that does not exist yet.
    pub base: Option<ObjectId>,
    /// The last changeset of the branch that the workspace was brought onto:
    /// where its own changesets start. A hint, checked before it is used.
    pub upstream: Option<ObjectId>,
    /// What its file system takes for the same name.
    pub fs: FileSystem,
    /// Set while a checkout is not finished.
    pub checkout: Option<Checkout>,
    /// Paths given to `braid track`.
    pub tracked: Vec<Vec<u8>>,
    /// The rebase in progress, if any.
    pub rebase: Option<Rebase>,
}

impl State {
    /// Reads the state file in `dir`.
    pub(crate) fn load(dir: &Path) -> Result<State, Error> {
        let path = dir.join(FILE);
        let bytes = fs::read(&path).map_err(Error::io("read", &path))?;
        decode(&bytes).map_err(|detail| Error::BadState { path, detail })
    }

    /// Writes the state file in `dir`, durably, through a scratch file and
    /// a rename.
    pub(crate) fn save(&self, dir: &Path) -> Result<(), Error> {
        let target = dir.join(FILE);
        let scratch = dir.join(format!("{FILE}.tmp-{}", random_hex()));
        let written = File::create_new(&scratch)
            .and_then(|mut file| {
                file.write_all(self.encode().as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(&scratch, &target))
            .and_then(|()| File::open(dir)?.sync_all());
        if written.is_err() {
            let _ = fs::remove_file(&scratch);
        }
        written.map_err(Error::io("write", &target))
    }

    fn encode(&self) -> String {
        let id = |id: Option<ObjectId>| id.map_or_else(|| "none".to_owned(), |id| id.to_hex());
        let yes = |flag: bool| if flag { "yes" } else { "no" };
        let mut text = format!(
            "{HEADER}\npath {}\nbranch {}\nbase {}\nupstream {}\nfs ignores-case={} ignores-normalization={}\n",
            escape(&path_bytes(&self.path).unwrap_or_default()),
            escape(self.branch.as_bytes()),
            id(self.base),
            id(self.upstream),
            yes(self.fs.ignores_case),
            yes(self.fs.ignores_normalization),
        );
        if let Some(Checkout { from, to }) = self.checkout {
            text.push_str(&format!("checkout {from} {to}\n"));
        }
        for path in &self.tracked {
            text.push_str(&format!("track {}\n", escape(path)));
        }
        if let Some(rebase) = &self.rebase {
            text.push_str(&format!("rebase-change {}\n", rebase.change));
            text.push_str(&format!("rebase-onto {}\n", rebase.onto));
            for pending in &rebase.pending {
                text.push_str(&format!("rebase-pending {pending}\n"));
            }
            for conflict in &rebase.conflicts {
                text.push_str(&format!(
                    "conflict {} {} {} {}\n",
                    conflict.code,
                    yes(conflict.markers),
                    yes(conflict.resolved),
                    escape(&conflict.path)
                ));
            }
        }
        text
    }
}

fn decode(bytes: &[u8]) -> Result<State, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "not UTF-8".to_owned())?;
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(format!("the first line is not {HEADER:?}"));
    }
    let mut state = State {
        path: PathBuf::new(),
        branch: String::new(),
        base: None,
        upstream: None,
        fs: FileSystem::FORGIVING,
        checkout: None,
        tracked: Vec::new(),
        rebase: None,
    };
    let mut change = None;
    let mut onto = None;
    let mut pending = Vec::new();
    let mut conflicts = Vec::new();
    for line in lines {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let bad = || format!("bad {key} line: {line:?}");
        let bytes = || unescape(value).ok_or_else(bad);
        let id = || value.parse::<ObjectId>().map_err(|_| bad());
        let maybe_id = || match value {
            "none" => Ok(None),
            _ => id().map(Some),
        };
        match key {
            "path" => state.path = path_from_bytes(&bytes()?).ok_or_else(bad)?,
            "branch" => state.branch = String::from_utf8(bytes()?).map_err(|_| bad())?,
            "base" => state.base = maybe_id()?,
            "upstream" => state.upstream = maybe_id()?,
            "fs" => {
                state.fs = FileSystem {
                    ignores_case: value.contains("ignores-case=yes"),
                    ignores_normalization: value.contains("ignores-normalization=yes"),
                }
            }
            "checkout" => {
                let (from, to) = value.split_once(' ').ok_or_else(bad)?;
                state.checkout = Some(Checkout {
                    from: from.parse().map_err(|_| bad())?,
                    to: to.parse().map_err(|_| bad())?,
                });
            }
            "track" => state.tracked.push(bytes()?),
            "rebase-change" => change = Some(id()?),
            "rebase-onto" => onto = Some(id()?),
            "rebase-pending" => pending.push(id()?),
            "conflict" => {
                let mut parts = value.splitn(4, ' ');
                let (Some(code), Some(markers), Some(resolved), Some(path)) =
                    (parts.next(), parts.next(), parts.next(), parts.next())
                else {
                    return Err(bad());
                };
                conflicts.push(RecordedConflict {
                    path: unescape(path).ok_or_else(bad)?,
                    code: code.to_owned(),
                    markers: markers == "yes",
                    resolved: resolved == "yes",
                });
            }
            // Later versions may add lines; this one ignores them.
            _ => {}
        }
    }
    if state.branch.is_empty() {
        return Err("no branch".to_owned());
    }
    match (change, onto) {
        (Some(change), Some(onto)) => {
            state.rebase = Some(Rebase {
                change,
                onto,
                pending,
                conflicts,
            });
        }
        (None, None) => {}
        _ => return Err("an incomplete rebase".to_owned()),
    }
    Ok(state)
}

/// The lock of a workspace, held until dropped.
#[derive(Debug)]
pub(crate) struct Lock {
    _file: File,
}

impl Lock {
    /// Waits for the lock of the workspace whose state is in `dir`.
    pub(crate) fn acquire(dir: &Path) -> Result<Lock, Error> {
        let path = dir.join(LOCK);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(Error::io("open", &path))?;
        file.lock().map_err(Error::io("lock", &path))?;
        Ok(Lock { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::Kind;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let a = ObjectId::hash(Kind::Changeset, b"a");
        let b = ObjectId::hash(Kind::Changeset, b"b");
        let mut state = State {
            path: PathBuf::from("/work/agent 1"),
            branch: "main".to_owned(),
            base: Some(a),
            upstream: None,
            fs: FileSystem {
                ignores_case: true,
                ignores_normalization: false,
            },
            checkout: Some(Checkout {
                from: ObjectId::hash(Kind::Tree, b"t"),
                to: ObjectId::hash(Kind::Tree, b"u"),
            }),
            tracked: vec![b".env".to_vec(), b"keys/id rsa".to_vec()],
            rebase: None,
        };
        state.save(dir.path()).unwrap();
        assert_eq!(State::load(dir.path()).unwrap(), state);

        state.rebase = Some(Rebase {
            change: a,
            onto: b,
            pending: vec![a, b],
            conflicts: vec![RecordedConflict {
                path: b"dir/with space\n".to_vec(),
                code: "CONTENT".to_owned(),
                markers: true,
                resolved: false,
            }],
        });
        state.checkout = None;
        state.save(dir.path()).unwrap();
        assert_eq!(State::load(dir.path()).unwrap(), state);
        // Only the state file is left.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        fs::write(
            dir.path().join(FILE),
            "braid-workspace-state 1\nbase nonsense\n",
        )
        .unwrap();
        assert!(matches!(
            State::load(dir.path()),
            Err(Error::BadState { .. })
        ));
    }

    #[test]
    fn the_lock_excludes_other_holders() {
        let dir = tempfile::tempdir().unwrap();
        let held = Lock::acquire(dir.path()).unwrap();
        let other = File::open(dir.path().join(LOCK)).unwrap();
        assert!(other.try_lock().is_err());
        drop(held);
        assert!(other.try_lock().is_ok());
    }
}
