// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Creating changesets, and the admission check a store runs before it
//! accepts one.

use braid_format::changeset::Assembled;
use braid_format::{Change, ChangeOp, ChangesetParts, Kind, ObjectId, Objects, Signature};

use crate::access::{
    EMPTY_TREE, Trees, empty_tree_node, read_changes, read_changeset, read_object,
};
use crate::apply::{apply, apply_sorted, check_nodes, first_problem};
use crate::error::{Error, store_error};

/// Creates a changeset: applies `changes` to the root tree of `parents[0]`
/// (the empty tree when there are no parents), stores the new directories,
/// the changeset, and its change-list blob when the list is too long to be
/// inline. Returns the changeset's id.
///
/// The blobs the changes refer to must be stored by the caller; the admission
/// check requires them.
pub fn commit<O: Objects>(
    objects: &O,
    parents: Vec<ObjectId>,
    changes: Vec<Change>,
    author: Signature,
    committer: Signature,
    message: Vec<u8>,
    extras: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<ObjectId, Error> {
    let base_root = match parents.first() {
        Some(parent) => read_changeset(objects, *parent)?.root,
        None => EMPTY_TREE,
    };
    let root = apply(objects, base_root, &changes)?;
    if root == EMPTY_TREE {
        // Not rebuilt when nothing changed, and never stored by a parent.
        objects
            .put(Kind::Tree, &empty_tree_node())
            .map_err(store_error)?;
    }
    let assembled = ChangesetParts {
        parents,
        root,
        changes,
        author,
        committer,
        message,
        extras,
        git_origin: None,
    }
    .assemble()
    .map_err(Error::Assemble)?;
    store_assembled(objects, &assembled)?;
    Ok(assembled.id)
}

/// Stores an assembled changeset, its change-list blob first.
pub(crate) fn store_assembled<O: Objects>(objects: &O, assembled: &Assembled) -> Result<(), Error> {
    if let Some((_, list)) = &assembled.external_list {
        objects.put(Kind::Blob, list).map_err(store_error)?;
    }
    objects
        .put(Kind::Changeset, &assembled.encoded)
        .map_err(store_error)?;
    Ok(())
}

/// The admission check: whether a store may accept the changeset `id`.
///
/// Every parent must already be admitted, as `is_admitted` says. The change
/// list must be canonical: sorted, without duplicates or no-ops, and stored
/// in its own blob only when it is too long to be inline. Applied to the root
/// tree of the first parent (or the empty tree), it must give exactly the root
/// the changeset declares, and no path may be deeper than
/// [`MAX_PATH_DEPTH`](crate::MAX_PATH_DEPTH), so no admitted tree is deeper
/// either. Every blob a change refers to must be stored, with the size the
/// change gives it, as must the blob of the original git commit, and every
/// directory the change rebuilds, byte for byte.
///
/// The cost follows the size of the change: only the directories on changed
/// paths are read, plus the blobs the changes refer to.
///
/// Returns `Ok(())` when the changeset is admissible. An error other than
/// [`Error::Store`] says why it is not.
pub fn admit<O: Objects>(
    objects: &O,
    id: ObjectId,
    is_admitted: impl Fn(&ObjectId) -> bool,
) -> Result<(), Error> {
    let changeset = read_changeset(objects, id)?;
    if let Some(parent) = changeset.parents.iter().find(|parent| !is_admitted(parent)) {
        return Err(Error::ParentNotAdmitted {
            changeset: id,
            parent: *parent,
        });
    }
    // Decoding checked the order, the paths, and the length of an inline list.
    let changes = read_changes(objects, id, &changeset)?;
    let base_root = match changeset.parents.first() {
        Some(parent) => read_changeset(objects, *parent)?.root,
        None => EMPTY_TREE,
    };

    let mut trees = Trees::new(objects);
    let sorted: Vec<&Change> = changes.iter().collect();
    let mut problems = Vec::new();
    let applied = apply_sorted(&mut trees, base_root, &sorted, &mut problems)?;
    if let Some(problem) = first_problem(problems) {
        return Err(problem.into_error());
    }
    if applied.root != changeset.root {
        return Err(Error::RootMismatch {
            changeset: id,
            declared: changeset.root,
            computed: applied.root,
        });
    }
    if applied.nodes.is_empty() {
        // Nothing was rebuilt: the root is the base root, which must be here.
        read_object(objects, changeset.root, Kind::Tree)?;
    }
    check_nodes(objects, &applied.nodes)?;

    for change in changes.iter() {
        if let ChangeOp::File { blob, size }
        | ChangeOp::Executable { blob, size }
        | ChangeOp::Symlink { blob, size } = &change.op
        {
            check_blob(objects, *blob, *size)?;
        }
    }
    if let Some(origin) = changeset.git_origin {
        read_object(objects, origin, Kind::Blob)?;
    }
    Ok(())
}

/// Checks that a blob is stored with the size an entry gives it.
fn check_blob<O: Objects>(objects: &O, blob: ObjectId, size: u64) -> Result<(), Error> {
    let actual = read_object(objects, blob, Kind::Blob)?.len() as u64;
    if actual != size {
        return Err(Error::BlobSize {
            blob,
            declared: size,
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::read_tree;
    use crate::error::ApplyProblem;
    use crate::path::MAX_PATH_DEPTH;
    use crate::testing::{Repo, deep_path, delete, signature, text_map};
    use braid_format::changeset::{MAX_INLINE_CHANGES_BYTES, encode_change_list};
    use braid_format::{Changes, Changeset, ChangesetError};

    fn admitted(ids: &[ObjectId]) -> impl Fn(&ObjectId) -> bool + '_ {
        move |id| ids.contains(id)
    }

    /// Stores a hand-made changeset and returns its id.
    fn forge(repo: &Repo, changeset: &Changeset) -> ObjectId {
        repo.objects
            .put(Kind::Changeset, &changeset.encode())
            .unwrap()
    }

    #[test]
    fn commit_builds_on_the_first_parent() {
        let repo = Repo::new();
        let first = repo.commit(&[], vec![repo.set("a.txt", "1"), repo.set("d/b.txt", "2")]);
        let second = repo.commit(&[first], vec![repo.set("a.txt", "3"), delete("d/b.txt")]);
        assert_eq!(repo.files(repo.root(second)), text_map(&[("a.txt", "3")]));
        let changeset = read_changeset(&repo.objects, second).unwrap();
        assert_eq!(changeset.parents, [first]);
        assert_eq!(changeset.author, signature("author"));
        assert_eq!(
            changeset.changes,
            Changes::Inline(vec![repo.set("a.txt", "3"), delete("d/b.txt")])
        );
        admit(&repo.objects, first, admitted(&[])).unwrap();
        admit(&repo.objects, second, admitted(&[first])).unwrap();
    }

    #[test]
    fn empty_commits_are_admissible() {
        let repo = Repo::new();
        let empty = repo.commit(&[], Vec::new());
        assert_eq!(repo.root(empty), EMPTY_TREE);
        admit(&repo.objects, empty, admitted(&[])).unwrap();
        let again = repo.commit(&[empty], Vec::new());
        admit(&repo.objects, again, admitted(&[empty])).unwrap();
        // Deleting everything gives the empty tree too.
        let one = repo.commit(&[empty], vec![repo.set("f", "x")]);
        let none = repo.commit(&[one], vec![delete("f")]);
        assert_eq!(repo.root(none), EMPTY_TREE);
        admit(&repo.objects, none, admitted(&[one])).unwrap();
    }

    #[test]
    fn commit_refuses_what_does_not_apply() {
        let repo = Repo::new();
        let first = repo.commit(&[], vec![repo.set("a", "1")]);
        let result = commit(
            &repo.objects,
            vec![first],
            vec![repo.set("a", "1")],
            signature("a"),
            signature("a"),
            Vec::new(),
            Vec::new(),
        );
        assert!(matches!(
            result,
            Err(Error::Apply {
                problem: ApplyProblem::NoOp,
                ..
            })
        ));
        let missing_parent = ObjectId::hash(Kind::Changeset, b"nowhere");
        let result = commit(
            &repo.objects,
            vec![missing_parent],
            Vec::new(),
            signature("a"),
            signature("a"),
            Vec::new(),
            Vec::new(),
        );
        assert!(matches!(
            result,
            Err(Error::Missing {
                kind: Kind::Changeset,
                ..
            })
        ));
    }

    #[test]
    fn admission_rejects_tampering() {
        let repo = Repo::new();
        let first = repo.commit(&[], vec![repo.set("a", "1"), repo.set("b", "2")]);
        let second = repo.commit(&[first], vec![repo.set("a", "3"), repo.set("c/d", "4")]);
        let good = read_changeset(&repo.objects, second).unwrap();

        // A parent that is not admitted.
        assert!(matches!(
            admit(&repo.objects, second, admitted(&[])),
            Err(Error::ParentNotAdmitted { parent, .. }) if parent == first
        ));

        // A wrong root.
        let mut wrong_root = good.clone();
        wrong_root.root = repo.root(first);
        assert!(matches!(
            admit(&repo.objects, forge(&repo, &wrong_root), admitted(&[first])),
            Err(Error::RootMismatch { .. })
        ));

        // A blob that is not stored.
        let unstored = Change {
            path: b"z".to_vec(),
            op: ChangeOp::File {
                blob: ObjectId::hash(Kind::Blob, b"never stored"),
                size: 12,
            },
        };
        let missing = commit(
            &repo.objects,
            vec![first],
            vec![unstored],
            signature("a"),
            signature("a"),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert!(matches!(
            admit(&repo.objects, missing, admitted(&[first])),
            Err(Error::Missing {
                kind: Kind::Blob,
                ..
            })
        ));

        // A blob with another size than the change says.
        let mut wrong_size = repo.set("z", "four");
        if let ChangeOp::File { size, .. } = &mut wrong_size.op {
            *size = 5;
        }
        let bad = commit(
            &repo.objects,
            vec![first],
            vec![wrong_size],
            signature("a"),
            signature("a"),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert!(matches!(
            admit(&repo.objects, bad, admitted(&[first])),
            Err(Error::BlobSize {
                declared: 5,
                actual: 4,
                ..
            })
        ));

        // An unsorted change list does not even decode.
        let mut unsorted = good.clone();
        unsorted.changes = Changes::Inline(vec![repo.set("c/d", "4"), repo.set("a", "3")]);
        assert!(matches!(
            admit(&repo.objects, forge(&repo, &unsorted), admitted(&[first])),
            Err(Error::Changeset {
                error: ChangesetError::Unsorted { .. },
                ..
            })
        ));

        // A no-op change, with the root it would give.
        let mut no_op = good.clone();
        no_op.changes = Changes::Inline(vec![repo.set("b", "2")]);
        no_op.root = repo.root(first);
        assert!(matches!(
            admit(&repo.objects, forge(&repo, &no_op), admitted(&[first])),
            Err(Error::Apply {
                problem: ApplyProblem::NoOp,
                ..
            })
        ));

        // A change list that leaves out a change: the root does not match.
        let mut partial = good.clone();
        partial.changes = Changes::Inline(vec![repo.set("a", "3")]);
        assert!(matches!(
            admit(&repo.objects, forge(&repo, &partial), admitted(&[first])),
            Err(Error::RootMismatch { .. })
        ));

        // A missing git origin.
        let mut origin = good.clone();
        origin.git_origin = Some(ObjectId::hash(Kind::Blob, b"raw commit"));
        assert!(matches!(
            admit(&repo.objects, forge(&repo, &origin), admitted(&[first])),
            Err(Error::Missing {
                kind: Kind::Blob,
                ..
            })
        ));

        // The untouched original is still fine.
        admit(&repo.objects, second, admitted(&[first])).unwrap();
    }

    #[test]
    fn admission_requires_the_new_directories() {
        let objects = braid_format::MemoryObjects::new();
        let repo = Repo::new();
        let first = repo.commit(&[], vec![repo.set("a/b", "1")]);
        let second = repo.commit(&[first], vec![repo.set("a/c", "2")]);
        // Copy everything except the directories `second` created.
        let new_dirs: Vec<ObjectId> = {
            let root = repo.root(second);
            let a = read_tree(&repo.objects, root)
                .unwrap()
                .into_iter()
                .find_map(|entry| match entry.kind {
                    braid_format::EntryKind::Tree { tree } => Some(tree),
                    _ => None,
                })
                .unwrap();
            vec![root, a]
        };
        for id in repo.objects.ids() {
            if !new_dirs.contains(&id) {
                let (kind, bytes) = repo.objects.get(&id).unwrap().unwrap();
                objects.put(kind, &bytes).unwrap();
            }
        }
        assert!(matches!(
            admit(&objects, second, admitted(&[first])),
            Err(Error::Missing {
                kind: Kind::Tree,
                ..
            })
        ));
    }

    #[test]
    fn long_change_lists_are_external() {
        let repo = Repo::new();
        let changes: Vec<Change> = (0..2000)
            .map(|i| repo.set(&format!("dir/file-{i:05}.txt"), "x"))
            .collect();
        assert!(encode_change_list(&changes).len() > MAX_INLINE_CHANGES_BYTES);
        let first = repo.commit(&[], changes.clone());
        let changeset = read_changeset(&repo.objects, first).unwrap();
        let Changes::External { list, count } = changeset.changes else {
            panic!("expected an external list");
        };
        assert_eq!(count, 2000);
        assert!(repo.objects.contains(&list).unwrap());
        assert_eq!(
            read_changes(&repo.objects, first, &changeset)
                .unwrap()
                .as_ref(),
            changes.as_slice()
        );
        admit(&repo.objects, first, admitted(&[])).unwrap();

        // A count that does not match the list.
        let mut miscounted = changeset.clone();
        miscounted.changes = Changes::External { list, count: 1999 };
        assert!(matches!(
            admit(&repo.objects, forge(&repo, &miscounted), admitted(&[])),
            Err(Error::ChangeCount { .. })
        ));

        // A short list stored externally is not canonical.
        let short = vec![repo.set("only", "x")];
        let short_list = repo
            .objects
            .put(Kind::Blob, &encode_change_list(&short))
            .unwrap();
        let mut external = changeset.clone();
        external.changes = Changes::External {
            list: short_list,
            count: 1,
        };
        external.root = repo.tree(&[("only", "x")]);
        assert!(matches!(
            admit(&repo.objects, forge(&repo, &external), admitted(&[])),
            Err(Error::ExternalListFitsInline { .. })
        ));
    }

    #[test]
    fn admission_enforces_the_depth_limit() {
        let repo = Repo::new();
        let deepest = deep_path(MAX_PATH_DEPTH);
        let first = repo.commit(&[], vec![repo.set(&deepest, "x")]);
        admit(&repo.objects, first, admitted(&[])).unwrap();

        // `commit` refuses a deeper path...
        let deeper = deep_path(MAX_PATH_DEPTH + 1);
        let result = commit(
            &repo.objects,
            vec![first],
            vec![repo.set(&deeper, "x")],
            signature("a"),
            signature("a"),
            Vec::new(),
            Vec::new(),
        );
        assert!(matches!(result, Err(Error::TooDeep { .. })), "{result:?}");
        // ...and a changeset made by hand with one is not admitted.
        let forged = ChangesetParts {
            parents: vec![first],
            root: repo.root(first),
            changes: vec![repo.set(&deeper, "x")],
            author: signature("a"),
            committer: signature("a"),
            message: Vec::new(),
            extras: Vec::new(),
            git_origin: None,
        }
        .assemble()
        .unwrap();
        store_assembled(&repo.objects, &forged).unwrap();
        let result = admit(&repo.objects, forged.id, admitted(&[first]));
        assert!(matches!(result, Err(Error::TooDeep { .. })), "{result:?}");
    }
}
