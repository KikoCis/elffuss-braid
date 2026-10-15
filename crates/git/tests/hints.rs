// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The hints an import gives the store: each new tree, blob and raw commit
//! is named similar to what the commit's first parent has in its place.

mod common;

use std::cell::RefCell;
use std::convert::Infallible;

use braid_format::{
    Changeset, Entry, EntryKind, Kind, MemoryObjects, ObjectId, Objects, read_directory,
};
use braid_git::{GitMap, GitOid, ImportOptions, import};
use common::{Repo, file};

/// Objects in memory that remember the hints they get: the id stored and
/// the id it was said to be similar to.
#[derive(Default)]
struct Hinted {
    objects: MemoryObjects,
    hints: RefCell<Vec<(ObjectId, ObjectId)>>,
}

impl Objects for Hinted {
    type Error = Infallible;

    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Self::Error> {
        self.objects.get(id)
    }

    fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Self::Error> {
        self.objects.put(kind, payload)
    }

    fn put_similar(
        &self,
        kind: Kind,
        payload: &[u8],
        similar: &ObjectId,
    ) -> Result<ObjectId, Self::Error> {
        let id = self.objects.put(kind, payload)?;
        self.hints.borrow_mut().push((id, *similar));
        Ok(id)
    }
}

fn text(version: u32) -> String {
    (0..50)
        .map(|n| {
            format!(
                "line {n} of version {}\n",
                if n == 25 { version } else { 0 }
            )
        })
        .collect()
}

fn changeset(objects: &Hinted, map: &GitMap, commit: &str) -> Changeset {
    let oid: GitOid = commit.parse().unwrap();
    let id = map.changeset(&oid).unwrap();
    let (_, bytes) = objects.get(&id).unwrap().unwrap();
    Changeset::decode(&bytes).unwrap()
}

fn entries(objects: &Hinted, tree: ObjectId) -> Vec<Entry> {
    read_directory(tree, |id| objects.get(id).unwrap().map(|(_, bytes)| bytes)).unwrap()
}

fn entry(entries: &[Entry], name: &str) -> EntryKind {
    entries
        .iter()
        .find(|entry| entry.name == name.as_bytes())
        .unwrap()
        .kind
        .clone()
}

#[test]
fn the_import_names_what_the_first_parent_has_in_place() {
    let mut git = Repo::new();
    let first = git.snapshot(
        "main",
        &[file("dir/a.txt", text(1)), file("b.txt", "same")],
        "first",
    );
    let second = git.snapshot(
        "main",
        &[file("dir/a.txt", text(2)), file("b.txt", "same")],
        "second",
    );
    let objects = Hinted::default();
    let mut map = GitMap::new();
    import(git.path(), &objects, &mut map, &ImportOptions::default()).unwrap();

    let before = changeset(&objects, &map, &first);
    let after = changeset(&objects, &map, &second);
    let old_root = entries(&objects, before.root);
    let new_root = entries(&objects, after.root);
    let tree = |kind| match kind {
        EntryKind::Tree { tree } => tree,
        other => panic!("{other:?}"),
    };
    let old_dir = tree(entry(&old_root, "dir"));
    let new_dir = tree(entry(&new_root, "dir"));
    let blob = |kind| match kind {
        EntryKind::File { blob, .. } => blob,
        other => panic!("{other:?}"),
    };
    let old_a = blob(entry(&entries(&objects, old_dir), "a.txt"));
    let new_a = blob(entry(&entries(&objects, new_dir), "a.txt"));

    let mut hints = objects.hints.take();
    hints.sort();
    let mut expected = vec![
        (after.root, before.root),
        (new_dir, old_dir),
        (new_a, old_a),
        (after.git_origin.unwrap(), before.git_origin.unwrap()),
    ];
    expected.sort();
    // The first commit has no parent to be like, and b.txt did not change.
    assert_eq!(hints, expected);
}
