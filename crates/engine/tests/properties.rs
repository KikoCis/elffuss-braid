// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Property tests on random small repositories: `diff` is canonical and
//! `apply` inverts it, admission accepts what `commit` makes and rejects
//! tampering, changes to unrelated paths land the same in either order, and
//! merging two changesets agrees with landing one on the other. Plus
//! properties of the three-way merge.
//!
//! Trees are built here from scratch with `build_directory`, independently of
//! the engine, and serve as the oracle.

use std::collections::{BTreeMap, BTreeSet};

use braid_engine::{
    ApplyProblem, Error, LandOutcome, MergeOutcome, MergeResult, admit, apply, commit, diff, land,
    merge, merge3, read_changes, read_changeset,
};
use braid_format::{
    Change, ChangeOp, Changes, Changeset, ChangesetError, ChangesetParts, Entry, EntryKind, GitId,
    Kind, MemoryObjects, ObjectId, Objects, Signature, build_directory,
};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;

/// What is at a path, before it is stored.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    File(Vec<u8>),
    Exec(Vec<u8>),
    Link(Vec<u8>),
    Gitlink(u8),
}

impl Value {
    fn kind(&self) -> EntryKind {
        let blob = |content: &[u8]| ObjectId::hash(Kind::Blob, content);
        match self {
            Value::File(c) => EntryKind::File {
                blob: blob(c),
                size: c.len() as u64,
            },
            Value::Exec(c) => EntryKind::Executable {
                blob: blob(c),
                size: c.len() as u64,
            },
            Value::Link(c) => EntryKind::Symlink {
                blob: blob(c),
                size: c.len() as u64,
            },
            Value::Gitlink(byte) => EntryKind::Gitlink {
                commit: GitId::Sha1([*byte; 20]),
            },
        }
    }

    fn op(&self) -> ChangeOp {
        ChangeOp::from_entry(&self.kind()).expect("a value is never a directory")
    }

    fn store(&self, objects: &MemoryObjects) {
        if let Value::File(c) | Value::Exec(c) | Value::Link(c) = self {
            objects.put(Kind::Blob, c).unwrap();
        }
    }
}

/// A repository state: every file, by path. No path is inside another.
type Files = BTreeMap<Vec<u8>, Value>;

fn store_all(objects: &MemoryObjects, files: &Files) {
    for value in files.values() {
        value.store(objects);
    }
}

/// Whether `prefix` is a directory that `path` is inside.
fn inside(prefix: &[u8], path: &[u8]) -> bool {
    path.len() > prefix.len() && path.starts_with(prefix) && path[prefix.len()] == b'/'
}

/// Whether two paths are the same, or one is inside the other.
fn related(a: &[u8], b: &[u8]) -> bool {
    a == b || inside(a, b) || inside(b, a)
}

/// The files that would stop `path` from being a file: files at its prefixes
/// and files inside it.
fn blockers(files: &Files, path: &[u8]) -> Vec<Vec<u8>> {
    let mut found: Vec<Vec<u8>> = path
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(|(at, _)| &path[..at])
        .filter(|prefix| files.contains_key(*prefix))
        .map(<[u8]>::to_vec)
        .collect();
    let mut from = path.to_vec();
    from.push(b'/');
    let mut to = path.to_vec();
    to.push(b'/' + 1);
    found.extend(files.range(from..to).map(|(key, _)| key.clone()));
    found
}

/// The canonical change list between two states, computed on the maps.
fn changes(old: &Files, new: &Files) -> Vec<Change> {
    let paths: BTreeSet<&Vec<u8>> = old.keys().chain(new.keys()).collect();
    paths
        .into_iter()
        .filter_map(|path| {
            let before = old.get(path).map(Value::kind);
            let after = new.get(path);
            (before != after.map(Value::kind)).then(|| Change {
                path: path.clone(),
                op: after.map_or(ChangeOp::Delete, Value::op),
            })
        })
        .collect()
}

/// Builds and stores the tree of `files` from scratch.
fn build(objects: &MemoryObjects, files: &Files) -> ObjectId {
    store_all(objects, files);
    let items: Vec<(&[u8], &Value)> = files.iter().map(|(p, v)| (p.as_slice(), v)).collect();
    build_dir(objects, &items)
}

fn build_dir(objects: &MemoryObjects, items: &[(&[u8], &Value)]) -> ObjectId {
    let mut entries = Vec::new();
    let mut below: BTreeMap<&[u8], Vec<(&[u8], &Value)>> = BTreeMap::new();
    for (path, value) in items {
        match path.iter().position(|byte| *byte == b'/') {
            None => entries.push(Entry {
                name: path.to_vec(),
                kind: value.kind(),
            }),
            Some(slash) => below
                .entry(&path[..slash])
                .or_default()
                .push((&path[slash + 1..], *value)),
        }
    }
    for (name, sub) in below {
        entries.push(Entry {
            name: name.to_vec(),
            kind: EntryKind::Tree {
                tree: build_dir(objects, &sub),
            },
        });
    }
    let directory = build_directory(entries).unwrap();
    for (_, bytes) in &directory.nodes {
        objects.put(Kind::Tree, bytes).unwrap();
    }
    directory.root()
}

fn signature() -> Signature {
    Signature {
        name: b"agent".to_vec(),
        email: b"agent@example.org".to_vec(),
        seconds: 1_700_000_000,
        offset_minutes: 0,
    }
}

fn commit_list(
    objects: &MemoryObjects,
    parents: Vec<ObjectId>,
    list: Vec<Change>,
) -> Result<ObjectId, Error> {
    commit(
        objects,
        parents,
        list,
        signature(),
        signature(),
        b"change\n".to_vec(),
        Vec::new(),
    )
}

/// Short names, so that paths collide, nest and sort around `/`, with a few
/// that differ only in case or normalisation.
fn name() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        8 => "[a-d]".prop_map(String::into_bytes),
        3 => "[a-c][a-c0-9-]{1,2}".prop_map(String::into_bytes),
        1 => Just(b"A".to_vec()),
        1 => Just(b"B".to_vec()),
        1 => Just("caf\u{e9}".as_bytes().to_vec()),
        1 => Just("cafe\u{301}".as_bytes().to_vec()),
    ]
}

fn path() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(name(), 1..=3).prop_map(|names| names.join(&b'/'))
}

fn content() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        Just(Vec::new()),
        Just(b"same\n".to_vec()),
        prop::collection::vec(
            prop_oneof![Just(&b"x\n"[..]), Just(&b"y\n"[..]), Just(&b"z"[..])],
            0..4
        )
        .prop_map(|parts| parts.concat()),
        prop::collection::vec(any::<u8>(), 0..8),
    ]
}

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        6 => content().prop_map(Value::File),
        2 => content().prop_map(Value::Exec),
        1 => content().prop_map(Value::Link),
        1 => (0u8..3).prop_map(Value::Gitlink),
    ]
}

/// A random repository; one time in five with a directory of about a
/// thousand files, on either side of the split limit.
fn files() -> impl Strategy<Value = Files> {
    (
        prop::collection::vec((path(), value()), 0..20),
        prop::option::weighted(0.2, (1016usize..1034, any::<u8>())),
    )
        .prop_map(|(list, wide)| {
            let mut files = Files::new();
            if let Some((count, salt)) = wide {
                for i in 0..count {
                    files.insert(
                        format!("w/f{i:04}").into_bytes(),
                        Value::File(vec![salt, (i % 5) as u8]),
                    );
                }
            }
            for (path, value) in list {
                if blockers(&files, &path).is_empty() {
                    files.insert(path, value);
                }
            }
            files
        })
}

#[derive(Clone, Debug)]
enum Edit {
    Modify(Index, Value),
    Delete(Index),
    /// Adds a file, deleting whatever is in its way.
    Add(Vec<u8>, Value),
}

fn edits() -> impl Strategy<Value = Vec<Edit>> {
    prop::collection::vec(
        prop_oneof![
            3 => (any::<Index>(), value()).prop_map(|(i, v)| Edit::Modify(i, v)),
            2 => any::<Index>().prop_map(Edit::Delete),
            3 => (path(), value()).prop_map(|(p, v)| Edit::Add(p, v)),
            1 => (1000u16..1060, value())
                .prop_map(|(n, v)| Edit::Add(format!("w/f{n:04}").into_bytes(), v)),
        ],
        0..10,
    )
}

fn edit(files: &Files, edits: &[Edit]) -> Files {
    let mut out = files.clone();
    for edit in edits {
        match edit {
            Edit::Modify(index, value) if !out.is_empty() => {
                let key = out.keys().nth(index.index(out.len())).unwrap().clone();
                out.insert(key, value.clone());
            }
            Edit::Delete(index) if !out.is_empty() => {
                let key = out.keys().nth(index.index(out.len())).unwrap().clone();
                out.remove(&key);
            }
            Edit::Add(path, value) => {
                for blocker in blockers(&out, path) {
                    out.remove(&blocker);
                }
                out.insert(path.clone(), value.clone());
            }
            _ => {}
        }
    }
    out
}

/// `target`, keeping only the changes to paths unrelated to `taken`, and
/// only those that still make a valid state without the others.
fn restrict(start: &Files, target: &Files, taken: &[Change]) -> Files {
    let wanted: Vec<Change> = changes(start, target)
        .into_iter()
        .filter(|change| !taken.iter().any(|t| related(&t.path, &change.path)))
        .collect();
    let mut files = start.clone();
    for change in &wanted {
        if matches!(change.op, ChangeOp::Delete) {
            files.remove(&change.path);
        }
    }
    for change in &wanted {
        if !matches!(change.op, ChangeOp::Delete) && blockers(&files, &change.path).is_empty() {
            files.insert(change.path.clone(), target[&change.path].clone());
        }
    }
    files
}

/// Lands `first` and then `second`, both built on `root`, and returns the
/// final root tree, or `None` if the second landing conflicts.
fn land_both(
    objects: &MemoryObjects,
    root: ObjectId,
    first: ObjectId,
    second: ObjectId,
) -> Result<Option<ObjectId>, TestCaseError> {
    let LandOutcome::FastForward(head) = land(objects, Some(root), first, signature())? else {
        return Err(TestCaseError::fail(
            "a change on the head must fast-forward",
        ));
    };
    let head_root = read_changeset(objects, head)?.root;
    match land(objects, Some(head), second, signature())? {
        LandOutcome::Rebased { changeset, .. } => {
            // What lands is admissible, and its list is the diff.
            admit(objects, changeset, |id| *id == head)?;
            let new = read_changeset(objects, changeset)?;
            let list = read_changes(objects, changeset, &new)?.into_owned();
            prop_assert_eq!(list, diff(objects, head_root, new.root)?);
            Ok(Some(new.root))
        }
        LandOutcome::AlreadyApplied => Ok(Some(head_root)),
        LandOutcome::Conflicts(_) => Ok(None),
        LandOutcome::FastForward(_) => Err(TestCaseError::fail("cannot fast-forward twice")),
    }
}

/// `diff` gives exactly the canonical list, and `apply` inverts it.
fn check_round_trip(old: &Files, new: &Files) -> Result<(), TestCaseError> {
    let objects = MemoryObjects::new();
    let a = build(&objects, old);
    let b = build(&objects, new);

    prop_assert_eq!(diff(&objects, a, a)?, Vec::new());
    prop_assert_eq!(diff(&objects, b, b)?, Vec::new());

    // Exactly the files that differ, sorted by path, without repeats.
    let forward = diff(&objects, a, b)?;
    prop_assert!(forward.windows(2).all(|pair| pair[0].path < pair[1].path));
    prop_assert_eq!(&forward, &changes(old, new));
    prop_assert_eq!(apply(&objects, a, &forward)?, b);

    let backward = diff(&objects, b, a)?;
    prop_assert_eq!(&backward, &changes(new, old));
    prop_assert_eq!(apply(&objects, b, &backward)?, a);
    Ok(())
}

/// Two changes to unrelated paths, built on `start`, land the same in either
/// order: both cleanly, with the same tree, or both with conflicts.
fn check_commutation(
    start: &Files,
    x_files: &Files,
    y_target: &Files,
) -> Result<(), TestCaseError> {
    let x_list = changes(start, x_files);
    let y_files = restrict(start, y_target, &x_list);
    let y_list = changes(start, &y_files);
    for x in &x_list {
        for y in &y_list {
            prop_assert!(!related(&x.path, &y.path));
        }
    }

    let objects = MemoryObjects::new();
    store_all(&objects, start);
    store_all(&objects, x_files);
    store_all(&objects, &y_files);
    let root = commit_list(&objects, Vec::new(), changes(&Files::new(), start))?;
    let x = commit_list(&objects, vec![root], x_list)?;
    let y = commit_list(&objects, vec![root], y_list.clone())?;

    let x_then_y = land_both(&objects, root, x, y)?;
    let y_then_x = land_both(&objects, root, y, x)?;
    prop_assert_eq!(x_then_y.is_some(), y_then_x.is_some());
    if let (Some(first), Some(second)) = (x_then_y, y_then_x) {
        prop_assert_eq!(first, second);
        let mut both = x_files.clone();
        for change in &y_list {
            match change.op {
                ChangeOp::Delete => {
                    both.remove(&change.path);
                }
                _ => {
                    both.insert(change.path.clone(), y_files[&change.path].clone());
                }
            }
        }
        prop_assert_eq!(first, build(&MemoryObjects::new(), &both));
    }
    Ok(())
}

/// Two changesets built on one start, related or not: merging the second
/// into the first gives what landing the second on the first gives, the same
/// conflicts or the same tree, and a merge that is admissible, whose change
/// list is the diff from the first.
fn check_merge_agrees_with_land(
    start: &Files,
    x_files: &Files,
    y_files: &Files,
) -> Result<(), TestCaseError> {
    let objects = MemoryObjects::new();
    store_all(&objects, start);
    store_all(&objects, x_files);
    store_all(&objects, y_files);
    let root = commit_list(&objects, Vec::new(), changes(&Files::new(), start))?;
    let x = commit_list(&objects, vec![root], changes(start, x_files))?;
    let y = commit(
        &objects,
        vec![root],
        changes(start, y_files),
        signature(),
        signature(),
        b"other change\n".to_vec(),
        Vec::new(),
    )?;
    let landed = land(&objects, Some(x), y, signature())?;
    let merged = merge(&objects, x, y, signature(), b"merge\n".to_vec())?;
    match (landed, merged) {
        (LandOutcome::Conflicts(by_land), MergeOutcome::Conflicts(by_merge)) => {
            prop_assert_eq!(by_land, by_merge);
        }
        (
            landed @ (LandOutcome::Rebased { .. } | LandOutcome::AlreadyApplied),
            MergeOutcome::Merged {
                changeset,
                merged_paths,
            },
        ) => {
            let new = read_changeset(&objects, changeset)?;
            prop_assert_eq!(&new.parents, &vec![x, y]);
            admit(&objects, changeset, |id| *id == x || *id == y)?;
            let x_root = read_changeset(&objects, x)?.root;
            let list = read_changes(&objects, changeset, &new)?.into_owned();
            prop_assert_eq!(list, diff(&objects, x_root, new.root)?);
            match landed {
                LandOutcome::Rebased {
                    changeset: rebased,
                    merged_paths: rebased_paths,
                } => {
                    prop_assert_eq!(new.root, read_changeset(&objects, rebased)?.root);
                    prop_assert_eq!(merged_paths, rebased_paths);
                }
                _ => prop_assert_eq!(new.root, x_root),
            }
        }
        (landed, merged) => {
            return Err(TestCaseError::fail(format!(
                "landing gave {landed:?}, merging gave {merged:?}"
            )));
        }
    }
    Ok(())
}

/// A directory of about a thousand files, on either side of the split
/// limit, and a few other files.
fn wide() -> impl Strategy<Value = Files> {
    (
        1000usize..1050,
        any::<u8>(),
        prop::collection::vec((path(), value()), 0..6),
    )
        .prop_map(|(count, salt, others)| {
            let mut files = Files::new();
            for i in 0..count {
                files.insert(
                    format!("w/f{i:04}").into_bytes(),
                    Value::File(vec![salt, (i % 3) as u8]),
                );
            }
            for (path, value) in others {
                if blockers(&files, &path).is_empty() {
                    files.insert(path, value);
                }
            }
            files
        })
}

/// Enough edits, mostly in the wide directory, to move it across the limit.
fn wide_edits() -> impl Strategy<Value = Vec<Edit>> {
    prop::collection::vec(
        prop_oneof![
            2 => (any::<Index>(), value()).prop_map(|(i, v)| Edit::Modify(i, v)),
            3 => any::<Index>().prop_map(Edit::Delete),
            3 => (1000u16..1080, value())
                .prop_map(|(n, v)| Edit::Add(format!("w/f{n:04}").into_bytes(), v)),
            1 => (path(), value()).prop_map(|(p, v)| Edit::Add(p, v)),
        ],
        0..40,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn diff_is_canonical_and_apply_inverts_it(old in files(), edits in edits()) {
        check_round_trip(&old, &edit(&old, &edits))?;
    }

    #[test]
    fn admission_accepts_commits_and_rejects_tampering(start in files(), edits in edits()) {
        let objects = MemoryObjects::new();
        // Oracle trees go to their own store, so they cannot stand in for a
        // directory that `commit` failed to write.
        let oracle = MemoryObjects::new();
        let new = edit(&start, &edits);
        store_all(&objects, &start);
        store_all(&objects, &new);
        let root = commit_list(&objects, Vec::new(), changes(&Files::new(), &start))?;
        admit(&objects, root, |_| false)?;
        let root_tree = read_changeset(&objects, root)?.root;
        prop_assert_eq!(root_tree, build(&oracle, &start));

        let list = changes(&start, &new);
        let child = commit_list(&objects, vec![root], list.clone())?;
        let good = read_changeset(&objects, child)?;
        prop_assert_eq!(good.root, build(&oracle, &new));
        let parent_only = |id: &ObjectId| *id == root;
        admit(&objects, child, parent_only)?;
        let not_admitted = admit(&objects, child, |_| false);
        prop_assert!(
            matches!(not_admitted, Err(Error::ParentNotAdmitted { .. })),
            "{:?}", not_admitted
        );

        let forge = |changeset: &Changeset| {
            objects.put(Kind::Changeset, &changeset.encode()).unwrap()
        };

        // A root the changes do not give.
        let mut wrong_root = good.clone();
        wrong_root.root = ObjectId::hash(Kind::Tree, b"another tree");
        let result = admit(&objects, forge(&wrong_root), parent_only);
        prop_assert!(matches!(result, Err(Error::RootMismatch { .. })), "{:?}", result);

        // A blob that is not stored.
        let mut missing = list.clone();
        missing.push(Change {
            path: b"zz-missing".to_vec(),
            op: ChangeOp::File {
                blob: ObjectId::hash(Kind::Blob, b"never stored"),
                size: 12,
            },
        });
        let id = commit_list(&objects, vec![root], missing)?;
        let result = admit(&objects, id, parent_only);
        prop_assert!(
            matches!(result, Err(Error::Missing { kind: Kind::Blob, .. })),
            "{:?}", result
        );

        // An unsorted list.
        if list.len() >= 2 && matches!(good.changes, Changes::Inline(_)) {
            let mut unsorted = good.clone();
            let mut swapped = list.clone();
            swapped.swap(0, 1);
            unsorted.changes = Changes::Inline(swapped);
            let result = admit(&objects, forge(&unsorted), parent_only);
            prop_assert!(
                matches!(
                    result,
                    Err(Error::Changeset { error: ChangesetError::Unsorted { .. }, .. })
                ),
                "{:?}", result
            );
        }

        // A no-op change, with the root it would give.
        if let Some((path, value)) = start.iter().next() {
            let no_op = ChangesetParts {
                parents: vec![root],
                root: root_tree,
                changes: vec![Change { path: path.clone(), op: value.op() }],
                author: signature(),
                committer: signature(),
                message: Vec::new(),
                extras: Vec::new(),
                git_origin: None,
            }
            .assemble()
            .unwrap();
            objects.put(Kind::Changeset, &no_op.encoded).unwrap();
            let result = admit(&objects, no_op.id, parent_only);
            prop_assert!(
                matches!(result, Err(Error::Apply { problem: ApplyProblem::NoOp, .. })),
                "{:?}", result
            );
        }
    }

    #[test]
    fn unrelated_changes_land_the_same_in_either_order(
        start in files(),
        x_edits in edits(),
        y_edits in edits(),
    ) {
        check_commutation(&start, &edit(&start, &x_edits), &edit(&start, &y_edits))?;
    }

    #[test]
    fn merging_agrees_with_landing(
        start in files(),
        x_edits in edits(),
        y_edits in edits(),
    ) {
        check_merge_agrees_with_land(&start, &edit(&start, &x_edits), &edit(&start, &y_edits))?;
    }
}

// Directories of about a thousand entries are slow to build and check in a
// debug build, so these run fewer cases.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn split_directories_round_trip_across_the_limit(old in wide(), edits in wide_edits()) {
        check_round_trip(&old, &edit(&old, &edits))?;
    }

    #[test]
    fn unrelated_changes_to_a_split_directory_commute(
        start in wide(),
        x_edits in wide_edits(),
        y_edits in wide_edits(),
    ) {
        check_commutation(&start, &edit(&start, &x_edits), &edit(&start, &y_edits))?;
    }
}

/// Text made of a few distinct lines, so that random versions share lines.
fn text() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            Just(&b"a\n"[..]),
            Just(&b"b\n"[..]),
            Just(&b"c\n"[..]),
            Just(&b"\n"[..]),
            Just(&b"  d\n"[..]),
            Just(&b"e"[..]),
        ],
        0..12,
    )
    .prop_map(|lines| lines.concat())
}

fn lines(text: &[u8]) -> Vec<&[u8]> {
    text.split_inclusive(|byte| *byte == b'\n').collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn merge_takes_the_side_that_changed(base in text(), other in text()) {
        let clean = MergeResult::Clean(other.clone());
        prop_assert_eq!(&merge3(&base, &base, &other), &clean);
        prop_assert_eq!(&merge3(&base, &other, &base), &clean);
        prop_assert_eq!(&merge3(&base, &other, &other), &clean);
    }

    #[test]
    fn merge_is_symmetric_and_hunks_match_the_inputs(
        base in text(),
        ours in text(),
        theirs in text(),
    ) {
        let forward = merge3(&base, &ours, &theirs);
        let backward = merge3(&base, &theirs, &ours);
        match (&forward, &backward) {
            (MergeResult::Clean(a), MergeResult::Clean(b)) => prop_assert_eq!(a, b),
            (MergeResult::Conflict { hunks: a }, MergeResult::Conflict { hunks: b }) => {
                prop_assert_eq!(a.len(), b.len());
                let (base_lines, ours_lines, theirs_lines) =
                    (lines(&base), lines(&ours), lines(&theirs));
                for (hunk, mirror) in a.iter().zip(b) {
                    prop_assert_eq!(&hunk.base, &mirror.base);
                    prop_assert_eq!(&hunk.ours, &mirror.theirs);
                    prop_assert_eq!(&hunk.theirs, &mirror.ours);
                    prop_assert_eq!(&hunk.base.text, &base_lines[hunk.base.lines.clone()].concat());
                    prop_assert_eq!(&hunk.ours.text, &ours_lines[hunk.ours.lines.clone()].concat());
                    prop_assert_eq!(
                        &hunk.theirs.text,
                        &theirs_lines[hunk.theirs.lines.clone()].concat()
                    );
                    prop_assert!(hunk.ours.text != hunk.theirs.text);
                }
            }
            _ => prop_assert!(false, "{:?} but {:?}", forward, backward),
        }
    }

    #[test]
    fn edits_with_a_line_between_them_merge(
        prefix in 0usize..5,
        ours_old in 0usize..4,
        gap in 1usize..4,
        theirs_old in 0usize..4,
        suffix in 0usize..5,
        ours_new in 0usize..3,
        theirs_new in 0usize..3,
    ) {
        // Distinct base lines; ours replaces [a, b) and theirs [c, d), with at
        // least one untouched line between b and c.
        let a = prefix;
        let b = a + ours_old;
        let c = b + gap;
        let d = c + theirs_old;
        let count = d + suffix;
        let base: Vec<Vec<u8>> = (0..count).map(|i| format!("line {i}\n").into_bytes()).collect();
        let replace = |from: usize, to: usize, tag: &str, n: usize| -> Vec<u8> {
            let mut out: Vec<u8> = base[..from].concat();
            for i in 0..n {
                out.extend(format!("{tag} {i}\n").into_bytes());
            }
            out.extend(base[to..].concat());
            out
        };
        let ours = replace(a, b, "ours", ours_new);
        let theirs = replace(c, d, "theirs", theirs_new);
        let mut expected: Vec<u8> = base[..a].concat();
        for i in 0..ours_new {
            expected.extend(format!("ours {i}\n").into_bytes());
        }
        expected.extend(base[b..c].concat());
        for i in 0..theirs_new {
            expected.extend(format!("theirs {i}\n").into_bytes());
        }
        expected.extend(base[d..].concat());
        let base: Vec<u8> = base.concat();
        prop_assert_eq!(merge3(&base, &ours, &theirs), MergeResult::Clean(expected.clone()));
        prop_assert_eq!(merge3(&base, &theirs, &ours), MergeResult::Clean(expected));
    }
}
