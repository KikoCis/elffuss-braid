// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! History: the log, changesets and their diffs, comparisons, branches and tags (section 15).
//!
//! Ids are 64 lowercase hex digits. The times of changesets are RFC 3339,
//! with the offset from UTC the changeset recorded. Text that a changeset
//! holds as bytes (names, messages, paths) comes as UTF-8, with what is not
//! UTF-8 replaced. A list holds at most one page, and `next`, when there is
//! more, is the opaque cursor that asks for the page after it (`?after=`).

use braid_format::ObjectId;
use serde::{Deserialize, Serialize};

use crate::hex;

/// The longest summary of a changeset, in characters.
pub const SUMMARY_CHARS: usize = 200;

/// Who wrote or recorded a changeset, and when.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// The name.
    pub name: String,
    /// The e-mail address.
    pub email: String,
    /// When, in RFC 3339, with the offset from UTC that was recorded.
    pub time: String,
}

/// A changeset in a list: the log, or what a comparison brings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesetSummary {
    /// The changeset.
    #[serde(with = "hex::id")]
    pub id: ObjectId,
    /// Its parents, the first one first.
    #[serde(with = "hex::ids")]
    pub parents: Vec<ObjectId>,
    /// Who wrote it.
    pub author: Signature,
    /// Who recorded it.
    pub committer: Signature,
    /// The first line of its message that is not blank, trimmed, at most
    /// [`SUMMARY_CHARS`] characters.
    pub summary: String,
    /// The agent that made it, when it names one.
    #[serde(default)]
    pub agent: Option<String>,
}

/// `GET …/log`: changesets along first parents, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Log {
    /// The revision the log starts from, as asked (the default branch when
    /// none was).
    #[serde(rename = "ref")]
    pub reference: String,
    /// The changesets of this page.
    pub changesets: Vec<ChangesetSummary>,
    /// The cursor of the next page, or `None` at the first changeset.
    #[serde(default)]
    pub next: Option<String>,
}

/// A file as one side of a change has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVersion {
    /// `file`, `executable`, `symlink` or `gitlink`.
    pub kind: String,
    /// The blob of its content (for a symlink, its target); `None` for a
    /// gitlink.
    #[serde(default, with = "hex::opt_id", skip_serializing_if = "Option::is_none")]
    pub blob: Option<ObjectId>,
    /// The size of its content, in bytes; `None` for a gitlink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// A gitlink's git commit, in hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// One path that a changeset, or a comparison, changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// The path, from the repository's root.
    pub path: String,
    /// `added`, `deleted`, `modified` (its content or its executable bit) or
    /// `type_changed` (between a file, a symlink and a gitlink).
    pub status: String,
    /// What was there before; `None` when it is added.
    pub old: Option<FileVersion>,
    /// What is there after; `None` when it is deleted.
    pub new: Option<FileVersion>,
}

/// `GET …/changesets/{rev}`: a changeset, and the files it changes against
/// its first parent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesetInfo {
    /// The changeset.
    #[serde(with = "hex::id")]
    pub id: ObjectId,
    /// Its parents, the first one first.
    #[serde(with = "hex::ids")]
    pub parents: Vec<ObjectId>,
    /// Its root tree.
    #[serde(with = "hex::id")]
    pub root: ObjectId,
    /// Who wrote it.
    pub author: Signature,
    /// Who recorded it.
    pub committer: Signature,
    /// Its whole message.
    pub message: String,
    /// The agent that made it, when it names one.
    #[serde(default)]
    pub agent: Option<String>,
    /// The changeset it was rebased from, when a landing rebased it.
    #[serde(default, with = "hex::opt_id")]
    pub rebased_from: Option<ObjectId>,
    /// The git commit it exports to, when the hub knows it already.
    #[serde(default)]
    pub git_commit: Option<String>,
    /// Its extra fields, in order, as `[name, value]`.
    #[serde(default)]
    pub extras: Vec<(String, String)>,
    /// The files it changes, in order of their paths' bytes.
    pub files: Vec<FileChange>,
    /// Whether it changes more files than `files` lists.
    #[serde(default)]
    pub files_truncated: bool,
}

/// One hunk of a file's diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// Its first line in the old version, counted from 1; with no old
    /// lines, the line it comes after (0 at the start).
    pub old_start: u64,
    /// How many lines of the old version it holds.
    pub old_lines: u64,
    /// Its first line in the new version, as `old_start` counts.
    pub new_start: u64,
    /// How many lines of the new version it holds.
    pub new_lines: u64,
    /// Its lines, in order, as `[op, text]`: `op` is `" "` for a line both
    /// versions have, `"-"` for one of the old version only and `"+"` for
    /// one of the new version only; `text` is the line, with its line
    /// ending when it has one.
    pub lines: Vec<(String, String)>,
}

/// One file of a diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// The path, from the repository's root.
    pub path: String,
    /// As [`FileChange::status`] says.
    pub status: String,
    /// What was there before; `None` when it is added.
    pub old: Option<FileVersion>,
    /// What is there after; `None` when it is deleted.
    pub new: Option<FileVersion>,
    /// Whether either version is binary, which is not diffed by lines. A
    /// file whose content stays the same (a new executable bit alone) has
    /// no lines to diff, and is neither binary nor truncated.
    #[serde(default)]
    pub binary: bool,
    /// The lines the new version adds.
    #[serde(default)]
    pub additions: u64,
    /// The lines it deletes.
    #[serde(default)]
    pub deletions: u64,
    /// The hunks, in order.
    #[serde(default)]
    pub hunks: Vec<Hunk>,
    /// Whether the hunks are cut short, or left out, to stay within the
    /// hub's sizes: ask for the file alone with `path` to get it whole when
    /// it fits.
    #[serde(default)]
    pub truncated: bool,
}

/// `GET …/changesets/{rev}/diff`: a page of a changeset's diff.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPage {
    /// The files of this page, in order of their paths' bytes.
    pub files: Vec<FileDiff>,
    /// The page after this one, when there are more files.
    #[serde(default)]
    pub next_page: Option<u64>,
}

/// `GET …/compare`: what a head brings to a base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comparison {
    /// The base's changeset.
    #[serde(with = "hex::id")]
    pub base: ObjectId,
    /// The head's changeset.
    #[serde(with = "hex::id")]
    pub head: ObjectId,
    /// Their best common ancestor (the one with the smallest id when there
    /// are several), or `None` when they share no history.
    #[serde(with = "hex::opt_id")]
    pub merge_base: Option<ObjectId>,
    /// The head's first-parent chain down to a changeset the base has,
    /// oldest first.
    pub stack: Vec<ChangesetSummary>,
    /// The rest of what the head has and the base lacks, which merges in
    /// the stack bring: parents before children.
    pub brings: Vec<ChangesetSummary>,
    /// The files the head changes since the merge base (since nothing, when
    /// there is none).
    pub files: Vec<FileChange>,
    /// Whether `stack`, `brings` or `files` is cut short to stay within the
    /// hub's sizes.
    #[serde(default)]
    pub truncated: bool,
}

/// A branch or a tag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefSummary {
    /// The ref, as a revision names it: `main`, `tags/v1`.
    pub name: String,
    /// The changeset it points to.
    #[serde(with = "hex::id")]
    pub head: ObjectId,
    /// When that changeset was recorded, as [`Signature::time`] says.
    pub time: String,
    /// Its summary, as [`ChangesetSummary::summary`] says.
    pub summary: String,
}

/// `GET …/branches` and `GET …/tags`, in order of their names.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefList {
    /// The refs of this page.
    pub refs: Vec<RefSummary>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::Kind;

    fn id(text: &str) -> ObjectId {
        ObjectId::hash(Kind::Changeset, text.as_bytes())
    }

    #[test]
    fn bodies_on_the_wire() {
        let log = Log {
            reference: "main".into(),
            changesets: vec![ChangesetSummary {
                id: id("two"),
                parents: vec![id("one")],
                author: Signature {
                    name: "Ada".into(),
                    email: "ada@example.org".into(),
                    time: "2023-11-14T22:13:20Z".into(),
                },
                committer: Signature::default(),
                summary: "Say why".into(),
                agent: Some("agent-1".into()),
            }],
            next: Some(id("one").to_hex()),
        };
        let text = serde_json::to_string(&log).unwrap();
        assert!(text.contains("\"ref\":\"main\""), "{text}");
        assert!(text.contains(&id("two").to_hex()), "{text}");
        assert_eq!(serde_json::from_str::<Log>(&text).unwrap(), log);

        // A gitlink has a commit and no blob; a file has a blob and a size.
        let gitlink = FileVersion {
            kind: "gitlink".into(),
            blob: None,
            size: None,
            commit: Some("ab".repeat(20)),
        };
        let text = serde_json::to_string(&gitlink).unwrap();
        assert!(!text.contains("blob") && !text.contains("size"), "{text}");
        assert_eq!(serde_json::from_str::<FileVersion>(&text).unwrap(), gitlink);

        let hunk = Hunk {
            old_start: 0,
            old_lines: 0,
            new_start: 1,
            new_lines: 1,
            lines: vec![("+".into(), "new\n".into())],
        };
        let text = serde_json::to_string(&hunk).unwrap();
        assert!(text.contains(r#""lines":[["+","new\n"]]"#), "{text}");
        assert_eq!(serde_json::from_str::<Hunk>(&text).unwrap(), hunk);

        // What a hub that says less gives still reads.
        let page: DiffPage = serde_json::from_str(
            r#"{"files":[{"path":"a","status":"added","old":null,"new":null}]}"#,
        )
        .unwrap();
        assert_eq!(page.next_page, None);
        assert!(!page.files[0].binary && page.files[0].hunks.is_empty());
        let refs: RefList = serde_json::from_str(r#"{"refs":[]}"#).unwrap();
        assert_eq!(refs, RefList::default());
    }
}
