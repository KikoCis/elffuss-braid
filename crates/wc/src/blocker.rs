// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! What stops a commit: the reasons `status` lists and `commit` refuses
//! with, so that an agent sees the same list before and after trying.

use std::fmt;

use crate::paths::show;
use crate::state::RecordedConflict;

/// One reason why `commit` would not record the working copy as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocker {
    /// Conflict markers remain in this file, from a content conflict of the
    /// rebase in progress. Editing them out settles it.
    Markers(Vec<u8>),
    /// A conflict of the rebase in progress that is not about content, and
    /// is not marked resolved: `braid resolve` marks it once it is settled.
    Unresolved {
        /// Where it is.
        path: Vec<u8>,
        /// Its kind, as [`braid_engine::ConflictKind::code`] names it.
        code: String,
    },
    /// A new file the secrets guard holds back until `braid track` marks
    /// it: its name looks like a secret, or it is large.
    NeedsTrack(Vec<u8>),
}

impl Blocker {
    /// A short, stable name for reports and JSON: `markers`, `unresolved`
    /// or `needs_track`.
    pub fn code(&self) -> &'static str {
        match self {
            Blocker::Markers(_) => "markers",
            Blocker::Unresolved { .. } => "unresolved",
            Blocker::NeedsTrack(_) => "needs_track",
        }
    }

    /// The path it is about.
    pub fn path(&self) -> &[u8] {
        match self {
            Blocker::Markers(path) | Blocker::NeedsTrack(path) => path,
            Blocker::Unresolved { path, .. } => path,
        }
    }

    /// The kind of the conflict it comes from, if it comes from one.
    pub fn conflict(&self) -> Option<&str> {
        match self {
            Blocker::Markers(_) => Some("CONTENT"),
            Blocker::Unresolved { code, .. } => Some(code),
            Blocker::NeedsTrack(_) => None,
        }
    }
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Blocker::Markers(path) => write!(
                f,
                "conflict markers remain in {:?}: edit them out",
                show(path)
            ),
            Blocker::Unresolved { path, code } => write!(
                f,
                "the {code} conflict at {:?} is not marked resolved: settle it, then run `braid resolve` on it",
                show(path)
            ),
            Blocker::NeedsTrack(path) => write!(
                f,
                "{:?} is a new file the secrets guard holds back: `braid track` it to commit it",
                show(path)
            ),
        }
    }
}

/// The reasons a commit of the paths `in_scope` accepts would be refused:
/// markers left in the files of content conflicts and other conflicts not
/// marked resolved, from `conflicts` (each with whether markers remain in
/// its file), then the new files of `needs_track`.
pub(crate) fn blockers<'p>(
    conflicts: &[(RecordedConflict, bool)],
    needs_track: impl IntoIterator<Item = &'p Vec<u8>>,
    in_scope: impl Fn(&[u8]) -> bool,
) -> Vec<Blocker> {
    let mut found = Vec::new();
    for (conflict, remain) in conflicts {
        if conflict.markers {
            if *remain {
                found.push(Blocker::Markers(conflict.path.clone()));
            }
        } else if !conflict.resolved {
            found.push(Blocker::Unresolved {
                path: conflict.path.clone(),
                code: conflict.code.clone(),
            });
        }
    }
    for path in needs_track {
        if in_scope(path) {
            found.push(Blocker::NeedsTrack(path.clone()));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict(path: &str, code: &str, markers: bool, resolved: bool) -> RecordedConflict {
        RecordedConflict {
            path: path.as_bytes().to_vec(),
            code: code.to_owned(),
            markers,
            resolved,
        }
    }

    #[test]
    fn reasons_in_order() {
        let conflicts = [
            (conflict("a", "CONTENT", true, false), true),
            (conflict("b", "CONTENT", true, false), false),
            (conflict("c", "MODIFY_DELETE", false, false), false),
            (conflict("d", "DF", false, true), false),
        ];
        let guarded = [b".env".to_vec(), b"keys/id_rsa".to_vec()];
        let found = blockers(&conflicts, &guarded, |path| path != b".env");
        assert_eq!(
            found,
            [
                Blocker::Markers(b"a".to_vec()),
                Blocker::Unresolved {
                    path: b"c".to_vec(),
                    code: "MODIFY_DELETE".to_owned()
                },
                Blocker::NeedsTrack(b"keys/id_rsa".to_vec()),
            ]
        );
        let codes: Vec<&str> = found.iter().map(Blocker::code).collect();
        assert_eq!(codes, ["markers", "unresolved", "needs_track"]);
        assert_eq!(found[1].conflict(), Some("MODIFY_DELETE"));
        assert!(found[0].to_string().contains("conflict markers remain"));
        assert!(found[1].to_string().contains("braid resolve"));
        assert!(found[2].to_string().contains("braid track"));
    }
}
