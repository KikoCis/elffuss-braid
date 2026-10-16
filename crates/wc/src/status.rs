// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! What differs between a working copy and its base, and the secrets guard.

use braid_format::ChangeOp;

use crate::paths::{is_within, split_last};
use crate::scan::Delta;
use crate::tree::same_class;

/// New files larger than this are not committed until `braid track` marks
/// them: 1 MiB.
pub const GUARD_SIZE: u64 = 1 << 20;

/// How a path differs from the base.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// A new file.
    Added,
    /// A file whose content or executable bit changed.
    Modified,
    /// A file that is gone.
    Deleted,
    /// A path that changed between a regular file, a symlink and a gitlink.
    TypeChanged,
}

impl ChangeKind {
    /// A short, stable name for reports and JSON.
    pub fn name(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
            ChangeKind::TypeChanged => "typechange",
        }
    }
}

/// One path that differs from the base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathChange {
    /// The path.
    pub path: Vec<u8>,
    /// How it differs.
    pub kind: ChangeKind,
    /// What committing it records: the file as it is now, or a delete.
    pub op: ChangeOp,
}

/// How a working copy differs from its base.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Every path that a commit would record, sorted by path.
    pub changes: Vec<PathChange>,
    /// New files held back by the secrets guard until `braid track` marks
    /// them, sorted.
    pub needs_track: Vec<Vec<u8>>,
}

impl Status {
    /// Whether nothing would be committed. Files waiting for `braid track`
    /// do not count: they are not committed until marked.
    pub fn is_clean(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Whether a new file must be marked with `braid track` before it is
/// committed: its name looks like a secret (`.env*`, `*.pem`, `id_*`, in
/// any letter case) or it is larger than [`GUARD_SIZE`].
pub fn is_guarded(path: &[u8], size: u64) -> bool {
    let name = split_last(path).1.to_ascii_lowercase();
    size > GUARD_SIZE
        || name.starts_with(b".env")
        || name.ends_with(b".pem")
        || name.starts_with(b"id_")
}

/// The status of a scan's deltas (see [`Delta`]). A new file that the
/// guard holds back is listed in `needs_track` instead, unless one of
/// `marks` (paths given to `braid track`) is it or a directory above it.
pub(crate) fn compare(deltas: &[Delta], marks: &[Vec<u8>]) -> Status {
    let mut status = Status::default();
    for delta in deltas {
        let path = &delta.path;
        match (&delta.old, &delta.new) {
            (Some(_), None) => status.changes.push(PathChange {
                path: path.clone(),
                kind: ChangeKind::Deleted,
                op: ChangeOp::Delete,
            }),
            (None, Some(new)) => {
                if is_guarded(path, size_of(new)) && !marks.iter().any(|mark| is_within(path, mark))
                {
                    status.needs_track.push(path.clone());
                } else {
                    status.changes.push(PathChange {
                        path: path.clone(),
                        kind: ChangeKind::Added,
                        op: new.clone(),
                    });
                }
            }
            (Some(old), Some(new)) if old != new => {
                let kind = if same_class(old, new) {
                    ChangeKind::Modified
                } else {
                    ChangeKind::TypeChanged
                };
                status.changes.push(PathChange {
                    path: path.clone(),
                    kind,
                    op: new.clone(),
                });
            }
            _ => {}
        }
    }
    status
}

fn size_of(op: &ChangeOp) -> u64 {
    match op {
        ChangeOp::File { size, .. }
        | ChangeOp::Executable { size, .. }
        | ChangeOp::Symlink { size, .. } => *size,
        ChangeOp::Gitlink { .. } | ChangeOp::Delete => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::{Kind, ObjectId};

    fn file(content: &str) -> ChangeOp {
        ChangeOp::File {
            blob: ObjectId::hash(Kind::Blob, content.as_bytes()),
            size: content.len() as u64,
        }
    }

    #[test]
    fn the_guard() {
        for guarded in [
            ".env",
            ".env.local",
            "config/.ENV",
            "key.pem",
            "id_rsa",
            "a/id_ed25519",
        ] {
            assert!(is_guarded(guarded.as_bytes(), 10), "{guarded}");
        }
        for plain in ["env", "README.md", "pem.txt", "my_id_rsa", "src/main.rs"] {
            assert!(!is_guarded(plain.as_bytes(), 10), "{plain}");
        }
        assert!(is_guarded(b"big.bin", GUARD_SIZE + 1));
        assert!(!is_guarded(b"exact.bin", GUARD_SIZE));
    }

    #[test]
    fn comparing() {
        let delta = |path: &str, old: Option<ChangeOp>, new: Option<ChangeOp>| Delta {
            path: path.as_bytes().to_vec(),
            old,
            new,
        };
        let target = ObjectId::hash(Kind::Blob, b"t");
        let link = ChangeOp::Symlink {
            blob: target,
            size: 1,
        };
        // Sorted by path, as a scan gives them.
        let deltas = [
            delta("edited", Some(file("1")), Some(file("2"))),
            delta("gone", Some(file("1")), None),
            delta("keys/id_rsa", None, Some(file("k"))),
            delta("link", Some(file("1")), Some(link)),
            delta("new", None, Some(file("3"))),
            delta("same", Some(file("1")), Some(file("1"))),
            delta("secrets/.env", None, Some(file("k=v"))),
        ];

        let status = compare(&deltas, &[b"keys".to_vec()]);
        let kinds: Vec<(&[u8], ChangeKind)> = status
            .changes
            .iter()
            .map(|c| (c.path.as_slice(), c.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                (&b"edited"[..], ChangeKind::Modified),
                (b"gone", ChangeKind::Deleted),
                (b"keys/id_rsa", ChangeKind::Added),
                (b"link", ChangeKind::TypeChanged),
                (b"new", ChangeKind::Added),
            ]
        );
        assert_eq!(status.changes[1].op, ChangeOp::Delete);
        assert_eq!(status.needs_track, [b"secrets/.env".to_vec()]);
        assert!(!status.is_clean());
    }
}
