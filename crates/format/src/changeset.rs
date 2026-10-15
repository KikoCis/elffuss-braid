// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Changesets: one change to the repository.
//!
//! A changeset names its parents, the root tree it produces, and the files
//! that changed relative to its **first** parent. The change list makes
//! history, blame and rebase cheap; the root tree id makes the changeset
//! commit to the whole repository state. A store only accepts a changeset
//! whose change list, applied to the first parent's tree, gives exactly that
//! root (see the admission check in the store).

use core::fmt;

use crate::codec::{DecodeError, Reader, put_bytes, put_id, put_sv, put_uv};
use crate::id::{Kind, ObjectId};
use crate::name::{PathError, forbidden_as_symlink, validate_path};
use crate::tree::{EntryKind, GitId};

/// Format byte of every changeset.
pub const FORMAT: u8 = 1;

/// A change list whose encoding is at most this many bytes is stored inside
/// the changeset; a longer one is stored as a separate blob.
pub const MAX_INLINE_CHANGES_BYTES: usize = 64 * 1024;

/// What happened to one path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChangeOp {
    /// The path no longer exists.
    Delete,
    /// The path is now this regular file.
    File { blob: ObjectId, size: u64 },
    /// The path is now this executable file.
    Executable { blob: ObjectId, size: u64 },
    /// The path is now this symlink.
    Symlink { blob: ObjectId, size: u64 },
    /// The path is now this git submodule.
    Gitlink { commit: GitId },
}

impl ChangeOp {
    /// The operation that sets a path to a tree entry of this kind. Trees are
    /// not files, so they have none: changes are always per file.
    pub fn from_entry(kind: &EntryKind) -> Option<Self> {
        Some(match kind {
            EntryKind::File { blob, size } => ChangeOp::File {
                blob: *blob,
                size: *size,
            },
            EntryKind::Executable { blob, size } => ChangeOp::Executable {
                blob: *blob,
                size: *size,
            },
            EntryKind::Symlink { blob, size } => ChangeOp::Symlink {
                blob: *blob,
                size: *size,
            },
            EntryKind::Gitlink { commit } => ChangeOp::Gitlink { commit: *commit },
            EntryKind::Tree { .. } => return None,
        })
    }

    /// The tree entry this operation sets, or `None` for a delete.
    pub fn to_entry(&self) -> Option<EntryKind> {
        Some(match self {
            ChangeOp::Delete => return None,
            ChangeOp::File { blob, size } => EntryKind::File {
                blob: *blob,
                size: *size,
            },
            ChangeOp::Executable { blob, size } => EntryKind::Executable {
                blob: *blob,
                size: *size,
            },
            ChangeOp::Symlink { blob, size } => EntryKind::Symlink {
                blob: *blob,
                size: *size,
            },
            ChangeOp::Gitlink { commit } => EntryKind::Gitlink { commit: *commit },
        })
    }
}

/// One changed path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Change {
    /// The full path, names joined by `/`.
    pub path: Vec<u8>,
    /// What happened to it.
    pub op: ChangeOp,
}

impl Change {
    fn encode(&self, out: &mut Vec<u8>) {
        put_bytes(out, &self.path);
        match &self.op {
            ChangeOp::Delete => out.push(0),
            ChangeOp::File { blob, size } => {
                out.push(1);
                put_id(out, blob);
                put_uv(out, *size);
            }
            ChangeOp::Executable { blob, size } => {
                out.push(2);
                put_id(out, blob);
                put_uv(out, *size);
            }
            ChangeOp::Symlink { blob, size } => {
                out.push(3);
                put_id(out, blob);
                put_uv(out, *size);
            }
            ChangeOp::Gitlink { commit } => {
                out.push(4);
                commit.encode(out);
            }
        }
    }

    /// Checks the path, and that a symlink does not take a name git or braid reads.
    fn validate(&self) -> Result<(), ChangesetError> {
        validate_path(&self.path).map_err(|error| ChangesetError::Path {
            path: self.path.clone(),
            error,
        })?;
        let last = self.path.rsplit(|byte| *byte == b'/').next().unwrap_or(&[]);
        if matches!(self.op, ChangeOp::Symlink { .. }) && forbidden_as_symlink(last) {
            return Err(ChangesetError::SymlinkName(self.path.clone()));
        }
        Ok(())
    }

    fn decode(reader: &mut Reader<'_>) -> Result<Self, ChangesetError> {
        let path = reader.bytes()?.to_vec();
        let at = reader.position();
        let op = match reader.u8()? {
            0 => ChangeOp::Delete,
            1 => ChangeOp::File {
                blob: reader.id()?,
                size: reader.uv()?,
            },
            2 => ChangeOp::Executable {
                blob: reader.id()?,
                size: reader.uv()?,
            },
            3 => ChangeOp::Symlink {
                blob: reader.id()?,
                size: reader.uv()?,
            },
            4 => ChangeOp::Gitlink {
                commit: GitId::decode(reader)?,
            },
            _ => {
                return Err(DecodeError::Invalid {
                    at,
                    what: "change operation",
                }
                .into());
            }
        };
        let change = Change { path, op };
        change.validate()?;
        Ok(change)
    }
}

/// Smallest possible encoded change: a 1-byte path and a delete.
const MIN_CHANGE_BYTES: usize = 1 + 1 + 1;

/// Who made or committed a change, and when.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Signature {
    /// Name, as raw bytes.
    pub name: Vec<u8>,
    /// E-mail address, as raw bytes.
    pub email: Vec<u8>,
    /// Seconds since the Unix epoch.
    pub seconds: i64,
    /// Offset from UTC in minutes.
    pub offset_minutes: i64,
}

impl Signature {
    fn encode(&self, out: &mut Vec<u8>) {
        put_bytes(out, &self.name);
        put_bytes(out, &self.email);
        put_sv(out, self.seconds);
        put_sv(out, self.offset_minutes);
    }

    fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Signature {
            name: reader.bytes()?.to_vec(),
            email: reader.bytes()?.to_vec(),
            seconds: reader.sv()?,
            offset_minutes: reader.sv()?,
        })
    }
}

/// Where the change list is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Changes {
    /// Inside the changeset.
    Inline(Vec<Change>),
    /// In a separate blob holding the encoded list.
    External {
        /// Id of the blob with the encoded list.
        list: ObjectId,
        /// Number of changes in it.
        count: u64,
    },
}

/// A decoded changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changeset {
    /// Parent changesets; the first is the base of the change list.
    pub parents: Vec<ObjectId>,
    /// Root tree of the repository after this change.
    pub root: ObjectId,
    /// The files that changed relative to the first parent.
    pub changes: Changes,
    /// Who wrote the change.
    pub author: Signature,
    /// Who recorded it.
    pub committer: Signature,
    /// The message, as raw bytes.
    pub message: Vec<u8>,
    /// Extra fields, in order, duplicates allowed.
    pub extras: Vec<(Vec<u8>, Vec<u8>)>,
    /// For a changeset imported from git: the blob with the original commit.
    pub git_origin: Option<ObjectId>,
}

/// Why a changeset or change list is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangesetError {
    /// The bytes are not a canonical encoding.
    Decode(DecodeError),
    /// A path is not valid.
    Path { path: Vec<u8>, error: PathError },
    /// Paths are not strictly ascending; `index` is the first out of order.
    Unsorted { index: usize },
    /// A symlink takes a name that git or braid reads.
    SymlinkName(Vec<u8>),
    /// An inline change list is longer than [`MAX_INLINE_CHANGES_BYTES`].
    InlineTooLong(usize),
}

impl fmt::Display for ChangesetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChangesetError::Decode(err) => write!(f, "{err}"),
            ChangesetError::Path { path, error } => {
                write!(f, "path {:?}: {error}", String::from_utf8_lossy(path))
            }
            ChangesetError::Unsorted { index } => {
                write!(f, "change {index} is out of order or a duplicate")
            }
            ChangesetError::SymlinkName(path) => write!(
                f,
                "symlink {:?} takes a name that must not be a symlink",
                String::from_utf8_lossy(path)
            ),
            ChangesetError::InlineTooLong(len) => write!(
                f,
                "inline change list of {len} bytes is longer than {MAX_INLINE_CHANGES_BYTES}"
            ),
        }
    }
}

impl std::error::Error for ChangesetError {}

impl From<DecodeError> for ChangesetError {
    fn from(err: DecodeError) -> Self {
        ChangesetError::Decode(err)
    }
}

fn check_ascending(changes: &[Change]) -> Result<(), ChangesetError> {
    for (index, pair) in changes.windows(2).enumerate() {
        if pair[0].path >= pair[1].path {
            return Err(ChangesetError::Unsorted { index: index + 1 });
        }
    }
    Ok(())
}

/// Encodes a change list: the count, then each change. This is both the
/// inline form and the content of an external change-list blob.
pub fn encode_change_list(changes: &[Change]) -> Vec<u8> {
    let mut out = Vec::new();
    put_uv(&mut out, changes.len() as u64);
    for change in changes {
        change.encode(&mut out);
    }
    out
}

fn read_change_list(reader: &mut Reader<'_>) -> Result<Vec<Change>, ChangesetError> {
    let count = reader.count(MIN_CHANGE_BYTES)?;
    let mut changes = Vec::with_capacity(count);
    for _ in 0..count {
        changes.push(Change::decode(reader)?);
    }
    check_ascending(&changes)?;
    Ok(changes)
}

/// Decodes the content of an external change-list blob.
pub fn decode_change_list(bytes: &[u8]) -> Result<Vec<Change>, ChangesetError> {
    let mut reader = Reader::new(bytes);
    let changes = read_change_list(&mut reader)?;
    reader.finish()?;
    Ok(changes)
}

/// Everything a changeset is made of, before the change list is placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangesetParts {
    /// Parent changesets; the first is the base of the change list.
    pub parents: Vec<ObjectId>,
    /// Root tree after the change.
    pub root: ObjectId,
    /// The changed paths, in any order.
    pub changes: Vec<Change>,
    /// Who wrote the change.
    pub author: Signature,
    /// Who recorded it.
    pub committer: Signature,
    /// The message.
    pub message: Vec<u8>,
    /// Extra fields.
    pub extras: Vec<(Vec<u8>, Vec<u8>)>,
    /// For an import from git: the blob with the original commit.
    pub git_origin: Option<ObjectId>,
}

/// A changeset in canonical form, ready to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assembled {
    /// The changeset.
    pub changeset: Changeset,
    /// Its id.
    pub id: ObjectId,
    /// Its encoding.
    pub encoded: Vec<u8>,
    /// The change-list blob to store alongside it, when the list is external.
    pub external_list: Option<(ObjectId, Vec<u8>)>,
}

impl ChangesetParts {
    /// Sorts and checks the change list, places it inline or in its own blob,
    /// and encodes the result.
    pub fn assemble(mut self) -> Result<Assembled, ChangesetError> {
        self.changes.sort_by(|a, b| a.path.cmp(&b.path));
        check_ascending(&self.changes)?;
        for change in &self.changes {
            change.validate()?;
        }
        let list = encode_change_list(&self.changes);
        let (changes, external_list) = if list.len() <= MAX_INLINE_CHANGES_BYTES {
            (Changes::Inline(self.changes), None)
        } else {
            let id = ObjectId::hash(Kind::Blob, &list);
            (
                Changes::External {
                    list: id,
                    count: self.changes.len() as u64,
                },
                Some((id, list)),
            )
        };
        let changeset = Changeset {
            parents: self.parents,
            root: self.root,
            changes,
            author: self.author,
            committer: self.committer,
            message: self.message,
            extras: self.extras,
            git_origin: self.git_origin,
        };
        let encoded = changeset.encode();
        let id = ObjectId::hash(Kind::Changeset, &encoded);
        Ok(Assembled {
            changeset,
            id,
            encoded,
            external_list,
        })
    }
}

impl Changeset {
    /// The canonical encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![FORMAT];
        put_uv(&mut out, self.parents.len() as u64);
        for parent in &self.parents {
            put_id(&mut out, parent);
        }
        put_id(&mut out, &self.root);
        match &self.changes {
            Changes::Inline(changes) => {
                out.push(0);
                out.extend_from_slice(&encode_change_list(changes));
            }
            Changes::External { list, count } => {
                out.push(1);
                put_id(&mut out, list);
                put_uv(&mut out, *count);
            }
        }
        self.author.encode(&mut out);
        self.committer.encode(&mut out);
        put_bytes(&mut out, &self.message);
        put_uv(&mut out, self.extras.len() as u64);
        for (key, value) in &self.extras {
            put_bytes(&mut out, key);
            put_bytes(&mut out, value);
        }
        match &self.git_origin {
            None => out.push(0),
            Some(id) => {
                out.push(1);
                put_id(&mut out, id);
            }
        }
        out
    }

    /// The id of this changeset.
    pub fn id(&self) -> ObjectId {
        ObjectId::hash(Kind::Changeset, &self.encode())
    }

    /// Decodes a changeset and checks everything that can be checked without
    /// other objects.
    pub fn decode(bytes: &[u8]) -> Result<Self, ChangesetError> {
        let mut reader = Reader::new(bytes);
        let at = reader.position();
        if reader.u8()? != FORMAT {
            return Err(DecodeError::Invalid {
                at,
                what: "changeset format",
            }
            .into());
        }
        let parent_count = reader.count(32)?;
        let mut parents = Vec::with_capacity(parent_count);
        for _ in 0..parent_count {
            parents.push(reader.id()?);
        }
        let root = reader.id()?;
        let at = reader.position();
        let changes = match reader.u8()? {
            0 => {
                let start = reader.position();
                let changes = read_change_list(&mut reader)?;
                let len = reader.position() - start;
                if len > MAX_INLINE_CHANGES_BYTES {
                    return Err(ChangesetError::InlineTooLong(len));
                }
                Changes::Inline(changes)
            }
            1 => Changes::External {
                list: reader.id()?,
                count: reader.uv()?,
            },
            _ => {
                return Err(DecodeError::Invalid {
                    at,
                    what: "change list mode",
                }
                .into());
            }
        };
        let author = Signature::decode(&mut reader)?;
        let committer = Signature::decode(&mut reader)?;
        let message = reader.bytes()?.to_vec();
        let extra_count = reader.count(2)?;
        let mut extras = Vec::with_capacity(extra_count);
        for _ in 0..extra_count {
            let key = reader.bytes()?.to_vec();
            let value = reader.bytes()?.to_vec();
            extras.push((key, value));
        }
        let at = reader.position();
        let git_origin = match reader.u8()? {
            0 => None,
            1 => Some(reader.id()?),
            _ => {
                return Err(DecodeError::Invalid {
                    at,
                    what: "git origin flag",
                }
                .into());
            }
        };
        reader.finish()?;
        Ok(Changeset {
            parents,
            root,
            changes,
            author,
            committer,
            message,
            extras,
            git_origin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::build_directory;

    fn blob(text: &str) -> ObjectId {
        ObjectId::hash(Kind::Blob, text.as_bytes())
    }

    fn signature() -> Signature {
        Signature {
            name: b"Ada Lovelace".to_vec(),
            email: b"ada@example.org".to_vec(),
            seconds: 1_700_000_000,
            offset_minutes: 60,
        }
    }

    fn parts(changes: Vec<Change>) -> ChangesetParts {
        ChangesetParts {
            parents: Vec::new(),
            root: build_directory(Vec::new()).unwrap().root(),
            changes,
            author: signature(),
            committer: signature(),
            message: b"Initial import\n".to_vec(),
            extras: Vec::new(),
            git_origin: None,
        }
    }

    fn set(path: &str, text: &str) -> Change {
        Change {
            path: path.as_bytes().to_vec(),
            op: ChangeOp::File {
                blob: blob(text),
                size: text.len() as u64,
            },
        }
    }

    #[test]
    fn golden_changeset() {
        let assembled = parts(vec![set("README.md", "hello\n"), set("src/lib.rs", "")])
            .assemble()
            .unwrap();
        assert!(assembled.external_list.is_none());
        assert_eq!(
            assembled.id.to_hex(),
            "4f8d49034a95a2938d30d13bee5508100211d04e1c125ef567a0c41c27f777d1"
        );
    }

    #[test]
    fn round_trip_with_everything() {
        let mut p = parts(vec![
            set("b/x", "1"),
            Change {
                path: b"a".to_vec(),
                op: ChangeOp::Delete,
            },
            Change {
                path: b"c".to_vec(),
                op: ChangeOp::Gitlink {
                    commit: GitId::Sha256([7; 32]),
                },
            },
        ]);
        p.parents = vec![blob("p1"), blob("p2")];
        p.extras = vec![
            (b"gpgsig".to_vec(), b"sig".to_vec()),
            (b"gpgsig".to_vec(), b"sig".to_vec()),
        ];
        p.git_origin = Some(blob("raw commit"));
        p.author.seconds = -5;
        p.committer.offset_minutes = -330;
        let assembled = p.assemble().unwrap();
        let decoded = Changeset::decode(&assembled.encoded).unwrap();
        assert_eq!(decoded, assembled.changeset);
        assert_eq!(decoded.encode(), assembled.encoded);
        assert_eq!(decoded.id(), assembled.id);
        match &decoded.changes {
            Changes::Inline(changes) => {
                let paths: Vec<&[u8]> = changes.iter().map(|c| c.path.as_slice()).collect();
                assert_eq!(paths, [&b"a"[..], b"b/x", b"c"]);
            }
            Changes::External { .. } => panic!("expected an inline list"),
        }
    }

    #[test]
    fn large_lists_go_to_a_blob() {
        let changes: Vec<Change> = (0..3000)
            .map(|i| set(&format!("dir/file-{i:06}.txt"), "x"))
            .collect();
        let assembled = parts(changes.clone()).assemble().unwrap();
        let (list_id, list) = assembled.external_list.clone().unwrap();
        assert_eq!(list_id, ObjectId::hash(Kind::Blob, &list));
        assert_eq!(decode_change_list(&list).unwrap(), changes);
        assert_eq!(
            assembled.changeset.changes,
            Changes::External {
                list: list_id,
                count: 3000
            }
        );
        let decoded = Changeset::decode(&assembled.encoded).unwrap();
        assert_eq!(decoded, assembled.changeset);
    }

    #[test]
    fn rejects_bad_lists() {
        let duplicate = parts(vec![set("a", "1"), set("a", "2")]).assemble();
        assert_eq!(duplicate, Err(ChangesetError::Unsorted { index: 1 }));
        let bad_path = parts(vec![set("a/../b", "1")]).assemble();
        assert!(matches!(bad_path, Err(ChangesetError::Path { .. })));
        let git_dir = parts(vec![set(".git/config", "1")]).assemble();
        assert!(matches!(git_dir, Err(ChangesetError::Path { .. })));
        let symlink = |path: &str| Change {
            path: path.as_bytes().to_vec(),
            op: ChangeOp::Symlink {
                blob: blob("target"),
                size: 6,
            },
        };
        assert_eq!(
            parts(vec![symlink("sub/.GITMODULES")]).assemble(),
            Err(ChangesetError::SymlinkName(b"sub/.GITMODULES".to_vec()))
        );
        assert!(
            parts(vec![symlink("sub/.gitmodules-notes")])
                .assemble()
                .is_ok()
        );

        // An inline list longer than the limit is not canonical.
        let changes: Vec<Change> = (0..3000)
            .map(|i| set(&format!("dir/file-{i:06}.txt"), "x"))
            .collect();
        let mut forged = parts(Vec::new()).assemble().unwrap().changeset;
        forged.changes = Changes::Inline(changes);
        assert!(matches!(
            Changeset::decode(&forged.encode()),
            Err(ChangesetError::InlineTooLong(_))
        ));

        let good = parts(vec![set("a", "1")]).assemble().unwrap().encoded;
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(matches!(
            Changeset::decode(&trailing),
            Err(ChangesetError::Decode(DecodeError::TrailingBytes { .. }))
        ));
        for cut in 0..good.len() {
            assert!(Changeset::decode(&good[..cut]).is_err(), "cut at {cut}");
        }
    }
}
