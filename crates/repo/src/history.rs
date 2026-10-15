// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! History queries over a repository: the engine's [`History`] on the store,
//! with the store's derived index.

use braid_engine::{DerivedIndex, DerivedKey, History};
use braid_format::{Kind, ObjectId, Objects};
use braid_store::{DerivedIndexFile, Store};

use crate::{Error, Repo};

/// The derived index of a repository's store, as the engine uses it.
///
/// Before it records entries, it makes everything this process wrote to the
/// store durable, so that the index never names a record a crash could lose.
#[derive(Clone, Copy, Debug)]
pub struct StoreIndex<'r> {
    store: &'r Store,
    file: &'r DerivedIndexFile,
}

impl DerivedIndex for StoreIndex<'_> {
    type Error = braid_store::Error;

    fn find(&self, key: &DerivedKey) -> Result<Option<ObjectId>, Self::Error> {
        self.file.find(key.as_bytes())
    }

    fn record(&self, entries: &[(DerivedKey, ObjectId)]) -> Result<(), Self::Error> {
        self.store.sync()?;
        let entries: Vec<([u8; 32], ObjectId)> = entries
            .iter()
            .map(|(key, record)| (*key.as_bytes(), *record))
            .collect();
        self.file.append(&entries)
    }
}

/// History queries over a repository's store.
pub type RepoHistory<'r> = History<'r, Store, StoreIndex<'r>>;

impl Repo {
    /// History queries over the store, finding derived data through the
    /// store's derived index and computing what is missing. Call
    /// [`History::flush`] when done, so that what was computed is kept.
    pub fn history(&self) -> RepoHistory<'_> {
        History::new(
            &self.store,
            StoreIndex {
                store: &self.store,
                file: &self.derived,
            },
        )
    }

    /// The changesets the refs point at, without repeats, sorted.
    pub fn ref_changesets(&self) -> Result<Vec<ObjectId>, Error> {
        let mut heads = Vec::new();
        for (_, id) in self.store.refs()?.iter() {
            if matches!(self.store.stat(&id)?, Some((Kind::Changeset, _))) {
                heads.push(id);
            }
        }
        heads.sort();
        heads.dedup();
        Ok(heads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::{Change, ChangeOp, Signature};
    use braid_store::RefUpdate;

    fn signature() -> Signature {
        Signature {
            name: b"Ada".to_vec(),
            email: b"ada@example.org".to_vec(),
            seconds: 1_700_000_000,
            offset_minutes: 0,
        }
    }

    fn change(repo: &Repo, parents: Vec<ObjectId>, content: &str) -> ObjectId {
        let blob = repo.store().put(Kind::Blob, content.as_bytes()).unwrap();
        let change = Change {
            path: b"f".to_vec(),
            op: ChangeOp::File {
                blob,
                size: content.len() as u64,
            },
        };
        braid_engine::commit(
            repo.store(),
            parents,
            vec![change],
            signature(),
            signature(),
            b"change\n".to_vec(),
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn derived_data_is_kept_between_processes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        let one = change(&repo, Vec::new(), "1\n");
        let two = change(&repo, vec![one], "1\n2\n");
        repo.store().sync().unwrap();
        repo.store()
            .transact("test", "main", |_| {
                Ok::<_, std::convert::Infallible>(vec![RefUpdate::new("main", None, Some(two))])
            })
            .unwrap();
        assert_eq!(repo.ref_changesets().unwrap(), [two]);

        let mut history = repo.history();
        let report = history.derive(&[two]).unwrap();
        assert_eq!(report.changesets, 2);
        assert_eq!(report.computed, 4);
        let blame = history.blame(two, b"f").unwrap().unwrap();
        assert_eq!(blame.origins, [one, two]);
        history.flush().unwrap();

        // Another process finds everything, and the store stays clean.
        let again = Repo::open(dir.path()).unwrap();
        let mut history = again.history();
        assert_eq!(history.derive(&[two]).unwrap().computed, 0);
        assert_eq!(
            history.blame(two, b"f").unwrap().unwrap().origins,
            [one, two]
        );
        assert_eq!(history.path_log(two, &[b"f"], 10).unwrap(), [two, one]);
        assert_eq!(history.computed(), 0);
        assert!(again.store().fsck().unwrap().is_clean());
    }
}
