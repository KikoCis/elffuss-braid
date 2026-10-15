// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Merging two changesets: a merge changeset whose tree is the three-way
//! merge of both trees against their merge base.
//!
//! The merge runs landing's rebase: the files that changed from the merge base
//! to `theirs` are applied to `ours` path by path, so every rule and every
//! kind of conflict is the one [`land`](crate::land) uses. Paths that only
//! `ours` changed keep their value without being looked at.

use braid_format::{ChangesetParts, Kind, ObjectId, Objects, Signature};

use crate::access::{EMPTY_TREE, empty_tree_node, read_changeset};
use crate::ancestry::merge_bases;
use crate::apply::write_nodes;
use crate::commit::store_assembled;
use crate::diff::diff;
use crate::error::{Error, store_error};
use crate::land::{Conflict, Rebase};

/// What merging one changeset into another gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// `theirs` is `ours` or one of its ancestors: there is nothing to
    /// merge. Nothing was stored.
    UpToDate,
    /// `ours` is an ancestor of `theirs`, which has everything already: move
    /// to it, without a merge changeset. Nothing was stored.
    FastForward(ObjectId),
    /// The merge changeset, stored.
    Merged {
        /// The new changeset, whose parents are `[ours, theirs]`.
        changeset: ObjectId,
        /// The paths both sides changed, merged line by line or by kind.
        merged_paths: Vec<Vec<u8>>,
    },
    /// The sides conflict. Nothing was stored.
    Conflicts(Vec<Conflict>),
}

/// Merges the changeset `theirs` into the changeset `ours`.
///
/// When one contains the other, the outcome says so and nothing is stored.
/// Otherwise the merge starts from their best common ancestor (see
/// [`merge_bases`](crate::merge_bases)). The merge changeset has the parents
/// `[ours, theirs]`; as root, the three-way merge of the three trees; the
/// change list against `ours`; `committer` as author and committer;
/// `message`; and `extras` as its extra fields, such as [`AGENT`](crate::AGENT)
/// naming who merged. It is made even when `ours` already has
/// every change of `theirs`, with an empty change list, so that its history
/// records the merge. Blobs, directories and the changeset are stored only
/// when the outcome is [`Merged`](MergeOutcome::Merged).
///
/// The conflicts are the ones [`land`](crate::land) reports for the same
/// trees, with `ours` as the head and `theirs` as the change. Changesets
/// without a common ancestor are not merged
/// ([`Error::UnrelatedHistories`]), and neither are changesets with several
/// best common ancestors whose trees differ ([`Error::SeveralMergeBases`]),
/// since the result would depend on which one the merge started from.
///
/// The cost follows the changes from the merge base to `theirs`, plus the
/// walks of history that find the merge base.
pub fn merge<O: Objects>(
    objects: &O,
    ours: ObjectId,
    theirs: ObjectId,
    committer: Signature,
    message: Vec<u8>,
    extras: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<MergeOutcome, Error> {
    let bases = merge_bases(objects, ours, theirs)?;
    let base_root = match bases.as_slice() {
        [] => return Err(Error::UnrelatedHistories { ours, theirs }),
        [base] if *base == theirs => return Ok(MergeOutcome::UpToDate),
        [base] if *base == ours => return Ok(MergeOutcome::FastForward(theirs)),
        [base] => read_changeset(objects, *base)?.root,
        several => shared_root(objects, ours, theirs, several)?,
    };
    let ours_root = read_changeset(objects, ours)?.root;
    let theirs_root = read_changeset(objects, theirs)?.root;

    // Their side as a change built on the base, rebased onto ours.
    let changes = diff(objects, base_root, theirs_root)?;
    let mut rebase = Rebase::new(objects, base_root, ours_root, theirs_root);
    for change in &changes {
        rebase.path(change)?;
    }
    let applied = rebase.structure()?;
    if !rebase.conflicts.is_empty() {
        return Ok(MergeOutcome::Conflicts(rebase.into_conflicts()));
    }

    for (_, bytes) in &rebase.blobs {
        objects.put(Kind::Blob, bytes).map_err(store_error)?;
    }
    write_nodes(objects, &applied.nodes)?;
    if applied.root == EMPTY_TREE {
        objects
            .put(Kind::Tree, &empty_tree_node())
            .map_err(store_error)?;
    }
    let assembled = ChangesetParts {
        parents: vec![ours, theirs],
        root: applied.root,
        changes: rebase.resolved,
        author: committer.clone(),
        committer,
        message,
        extras,
        git_origin: None,
    }
    .assemble()
    .map_err(Error::Assemble)?;
    store_assembled(objects, &assembled)?;
    Ok(MergeOutcome::Merged {
        changeset: assembled.id,
        merged_paths: rebase.merged,
    })
}

/// The root tree of several merge bases, when they all have the same one:
/// then the merge does not depend on which of them it starts from.
fn shared_root<O: Objects>(
    objects: &O,
    ours: ObjectId,
    theirs: ObjectId,
    bases: &[ObjectId],
) -> Result<ObjectId, Error> {
    let mut roots = Vec::with_capacity(bases.len());
    for base in bases {
        roots.push(read_changeset(objects, *base)?.root);
    }
    match roots.split_first() {
        Some((first, rest)) if rest.iter().all(|root| root == first) => Ok(*first),
        _ => Err(Error::SeveralMergeBases {
            ours,
            theirs,
            bases: bases.to_vec(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::read_changes;
    use crate::commit::{admit, commit};
    use crate::land::{AGENT, ConflictKind, LandOutcome, land};
    use crate::testing::{Repo, delete, signature, text_map};
    use braid_format::{Change, Changes};

    /// A base with `files`, and two changesets on it: ours applies `ours`
    /// and theirs applies `theirs`.
    struct Fork {
        repo: Repo,
        ours: ObjectId,
        theirs: ObjectId,
    }

    fn fork(
        files: &[(&str, &str)],
        ours: impl FnOnce(&Repo) -> Vec<Change>,
        theirs: impl FnOnce(&Repo) -> Vec<Change>,
    ) -> Fork {
        let repo = Repo::new();
        let start: Vec<Change> = files.iter().map(|(p, c)| repo.set(p, c)).collect();
        let base = repo.commit(&[], start);
        let ours = repo.commit(&[base], ours(&repo));
        let theirs = commit(
            &repo.objects,
            vec![base],
            theirs(&repo),
            signature("other"),
            signature("other"),
            b"theirs\n".to_vec(),
            Vec::new(),
        )
        .unwrap();
        Fork { repo, ours, theirs }
    }

    fn merge_it(fork: &Fork) -> MergeOutcome {
        merge(
            &fork.repo.objects,
            fork.ours,
            fork.theirs,
            signature("merger"),
            b"Merge\n".to_vec(),
            vec![(AGENT.to_vec(), b"merger".to_vec())],
        )
        .unwrap()
    }

    /// Merges a fork that must merge cleanly, and checks the result: it is
    /// admissible, and its change list is the diff from ours.
    fn merged(fork: &Fork) -> (ObjectId, Vec<Vec<u8>>) {
        let outcome = merge_it(fork);
        let MergeOutcome::Merged {
            changeset,
            merged_paths,
        } = outcome
        else {
            panic!("expected a merge, got {outcome:?}");
        };
        let objects = &fork.repo.objects;
        admit(objects, changeset, |id| {
            [fork.ours, fork.theirs].contains(id)
        })
        .unwrap();
        let new = read_changeset(objects, changeset).unwrap();
        assert_eq!(new.parents, [fork.ours, fork.theirs]);
        let list = read_changes(objects, changeset, &new).unwrap().into_owned();
        assert_eq!(
            list,
            diff(objects, fork.repo.root(fork.ours), new.root).unwrap()
        );
        (changeset, merged_paths)
    }

    #[test]
    fn one_side_containing_the_other() {
        let repo = Repo::new();
        let base = repo.commit(&[], vec![repo.set("a", "1")]);
        let next = repo.commit(&[base], vec![repo.set("a", "2")]);
        let later = repo.commit(&[next], vec![repo.set("b", "1")]);
        let objects = &repo.objects;
        let stored = objects.len();
        let run = |ours, theirs| {
            merge(
                objects,
                ours,
                theirs,
                signature("m"),
                Vec::new(),
                Vec::new(),
            )
            .unwrap()
        };
        assert_eq!(run(base, later), MergeOutcome::FastForward(later));
        assert_eq!(run(later, base), MergeOutcome::UpToDate);
        assert_eq!(run(later, next), MergeOutcome::UpToDate);
        assert_eq!(run(later, later), MergeOutcome::UpToDate);
        assert_eq!(objects.len(), stored, "nothing is stored");
    }

    #[test]
    fn disjoint_changes_merge() {
        let f = fork(
            &[("a", "1"), ("d/b", "2"), ("gone", "x"), ("old/x", "1")],
            |r| vec![r.set("a", "ours"), r.set("d/new", "o"), delete("gone")],
            |r| vec![r.set("d/b", "theirs"), r.set("e/f", "t"), delete("old/x")],
        );
        let (id, merged_paths) = merged(&f);
        assert!(merged_paths.is_empty());
        assert_eq!(
            f.repo.files(f.repo.root(id)),
            text_map(&[
                ("a", "ours"),
                ("d/b", "theirs"),
                ("d/new", "o"),
                ("e/f", "t")
            ])
        );
        let new = read_changeset(&f.repo.objects, id).unwrap();
        assert_eq!(new.author, signature("merger"));
        assert_eq!(new.committer, signature("merger"));
        assert_eq!(new.message, b"Merge\n");
        assert_eq!(new.extras, [(AGENT.to_vec(), b"merger".to_vec())]);
        assert_eq!(new.git_origin, None);

        // Merging the other way gives the same tree.
        let back = merge(
            &f.repo.objects,
            f.theirs,
            f.ours,
            signature("merger"),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let MergeOutcome::Merged { changeset, .. } = back else {
            panic!("expected a merge, got {back:?}");
        };
        assert_eq!(f.repo.root(changeset), new.root);
    }

    #[test]
    fn one_file_changed_on_both_sides_merges_line_by_line() {
        let f = fork(
            &[("f", "1\n2\n3\n4\n5\n")],
            |r| vec![r.set("f", "ONE\n2\n3\n4\n5\n")],
            |r| vec![r.exec("f", "1\n2\n3\n4\nFIVE\n")],
        );
        let (id, merged_paths) = merged(&f);
        assert_eq!(merged_paths, [b"f".to_vec()]);
        assert_eq!(
            f.repo.files(f.repo.root(id)),
            text_map(&[("f", "exec:ONE\n2\n3\n4\nFIVE\n")])
        );
    }

    #[test]
    fn a_merge_that_changes_no_file_is_still_made() {
        let f = fork(
            &[("a", "1")],
            |r| vec![r.set("a", "2"), r.set("b", "1")],
            |r| vec![r.set("a", "2")],
        );
        let (id, _) = merged(&f);
        let new = read_changeset(&f.repo.objects, id).unwrap();
        assert_eq!(new.root, f.repo.root(f.ours));
        assert_eq!(new.changes, Changes::Inline(Vec::new()));
    }

    #[test]
    fn deleting_everything() {
        let f = fork(
            &[("a", "1"), ("b", "1")],
            |_| vec![delete("a")],
            |_| vec![delete("b")],
        );
        let (id, _) = merged(&f);
        assert_eq!(f.repo.root(id), EMPTY_TREE);
        assert!(f.repo.objects.contains(&EMPTY_TREE).unwrap());
    }

    #[test]
    fn conflicts_are_the_ones_landing_finds() {
        let cases: Vec<Fork> = vec![
            fork(
                &[("f", "1\n2\n3\n")],
                |r| vec![r.set("f", "1\nours\n3\n")],
                |r| vec![r.set("f", "1\ntheirs\n3\n")],
            ),
            fork(
                &[("f", "1"), ("g", "1")],
                |r| vec![r.set("f", "2"), delete("g")],
                |r| vec![delete("f"), r.set("g", "2")],
            ),
            fork(
                &[("a", "1")],
                |r| vec![r.set("x/y", "o")],
                |r| vec![r.set("x", "t")],
            ),
            fork(
                &[("d/a", "1"), ("k", "1")],
                |_| vec![delete("d/a")],
                |r| vec![r.set("d/b", "2")],
            ),
            fork(
                &[("x", "1")],
                |r| vec![r.set("README", "o")],
                |r| vec![r.set("readme", "t")],
            ),
            fork(
                &[("f", "1")],
                |r| vec![r.symlink("f", "target")],
                |r| vec![r.exec("f", "1")],
            ),
        ];
        let mut kinds = Vec::new();
        for f in &cases {
            let stored = f.repo.objects.len();
            let MergeOutcome::Conflicts(found) = merge_it(f) else {
                panic!("expected conflicts");
            };
            assert_eq!(f.repo.objects.len(), stored, "nothing is stored");
            let landed = land(&f.repo.objects, Some(f.ours), f.theirs, signature("x")).unwrap();
            assert_eq!(landed, LandOutcome::Conflicts(found.clone()));
            kinds.extend(found.iter().map(|c| c.kind.code()));
        }
        assert_eq!(
            kinds,
            [
                "CONTENT",
                "MODIFY_DELETE",
                "MODIFY_DELETE",
                "DF",
                "DIR_REMOVED",
                "CASE",
                "MODE"
            ]
        );
        let MergeOutcome::Conflicts(found) = merge_it(&cases[3]) else {
            panic!("expected conflicts");
        };
        assert_eq!(
            found[0].kind,
            ConflictKind::DirectoryRemoved {
                directory: b"d".to_vec()
            }
        );
    }

    #[test]
    fn merging_again_after_a_merge() {
        // The history `git sync` makes: ours and theirs diverge, are merged,
        // and both sides go on.
        let f = fork(
            &[("a", "1"), ("b", "1")],
            |r| vec![r.set("a", "ours")],
            |r| vec![r.set("b", "theirs")],
        );
        let (first, _) = merged(&f);
        let repo = &f.repo;
        let run = |ours, theirs| {
            merge(
                &repo.objects,
                ours,
                theirs,
                signature("m"),
                Vec::new(),
                Vec::new(),
            )
        };
        assert_eq!(run(first, f.theirs).unwrap(), MergeOutcome::UpToDate);
        assert_eq!(
            run(f.theirs, first).unwrap(),
            MergeOutcome::FastForward(first)
        );

        let ours = repo.commit(&[first], vec![repo.set("c", "ours")]);
        let theirs = repo.commit(&[f.theirs], vec![repo.set("b", "again")]);
        let MergeOutcome::Merged { changeset, .. } = run(ours, theirs).unwrap() else {
            panic!("expected a merge");
        };
        assert_eq!(
            repo.files(repo.root(changeset)),
            text_map(&[("a", "ours"), ("b", "again"), ("c", "ours")])
        );
    }

    #[test]
    fn unrelated_histories_are_not_merged() {
        let repo = Repo::new();
        let one = repo.commit(&[], vec![repo.set("a", "1")]);
        let two = repo.commit(&[], vec![repo.set("b", "1")]);
        let result = merge(
            &repo.objects,
            one,
            two,
            signature("m"),
            Vec::new(),
            Vec::new(),
        );
        assert!(
            matches!(result, Err(Error::UnrelatedHistories { ours, theirs }) if ours == one && theirs == two),
            "{result:?}"
        );
    }

    /// A changeset on `parents` with `changes`, told apart by `message`.
    fn node(repo: &Repo, parents: &[ObjectId], changes: Vec<Change>, message: &str) -> ObjectId {
        commit(
            &repo.objects,
            parents.to_vec(),
            changes,
            signature("author"),
            signature("author"),
            message.as_bytes().to_vec(),
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn criss_cross_histories() {
        // Two sides merge each other: `x` and `y` are both best common
        // ancestors of what comes after.
        let repo = Repo::new();
        let root = node(&repo, &[], vec![repo.set("r", "1")], "root");
        let x = node(&repo, &[root], vec![repo.set("a", "x")], "x");
        let y = node(&repo, &[root], vec![repo.set("b", "y")], "y");
        let m1 = node(&repo, &[x, y], vec![repo.set("b", "y")], "m1");
        let m2 = node(&repo, &[y, x], vec![repo.set("a", "x")], "m2");
        let ours = node(&repo, &[m1], vec![repo.set("c", "1")], "ours");
        let theirs = node(&repo, &[m2], vec![repo.set("d", "1")], "theirs");
        let result = merge(
            &repo.objects,
            ours,
            theirs,
            signature("m"),
            Vec::new(),
            Vec::new(),
        );
        assert!(
            matches!(&result, Err(Error::SeveralMergeBases { bases, .. }) if bases.len() == 2),
            "{result:?}"
        );

        // When the best common ancestors have the same tree, the merge does
        // not depend on which one it starts from.
        let x = node(&repo, &[root], vec![repo.set("a", "1")], "x again");
        let y = node(&repo, &[root], vec![repo.set("a", "1")], "y again");
        let m1 = node(&repo, &[x, y], Vec::new(), "m1 again");
        let m2 = node(&repo, &[y, x], Vec::new(), "m2 again");
        let ours = node(&repo, &[m1], vec![repo.set("c", "1")], "ours again");
        let theirs = node(&repo, &[m2], vec![repo.set("d", "1")], "theirs again");
        let MergeOutcome::Merged { changeset, .. } = merge(
            &repo.objects,
            ours,
            theirs,
            signature("m"),
            Vec::new(),
            Vec::new(),
        )
        .unwrap() else {
            panic!("expected a merge");
        };
        assert_eq!(
            repo.files(repo.root(changeset)),
            text_map(&[("a", "1"), ("c", "1"), ("d", "1"), ("r", "1")])
        );
    }
}
