// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Reading and writing braid objects, with kinds checked and errors converted.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use braid_format::{Changeset, Entry, Kind, ObjectId, Objects, read_directory};

use crate::error::Error;

/// How many entries the directories a [`TreeCache`] keeps since its last
/// drop may hold before the ones not used since are dropped.
const TREE_CACHE_ENTRIES: usize = 1 << 17;

/// Fetches an object that must be of `kind`.
pub(crate) fn get<O: Objects>(objects: &O, id: &ObjectId, kind: Kind) -> Result<Vec<u8>, Error> {
    match objects.get(id).map_err(Error::objects)? {
        Some((found, payload)) if found == kind => Ok(payload),
        Some((found, _)) => Err(Error::BadBraidObject {
            id: *id,
            detail: format!("is a {found}, expected a {kind}"),
        }),
        None => Err(Error::MissingBraidObject {
            id: *id,
            expected: kind,
        }),
    }
}

/// Stores an object and checks the id the store gives back.
pub(crate) fn put<O: Objects>(objects: &O, kind: Kind, payload: &[u8]) -> Result<ObjectId, Error> {
    put_hinted(objects, kind, payload, None)
}

/// Stores an object, with an object it is probably close to as a hint for
/// the store, and checks the id the store gives back.
pub(crate) fn put_hinted<O: Objects>(
    objects: &O,
    kind: Kind,
    payload: &[u8],
    similar: Option<&ObjectId>,
) -> Result<ObjectId, Error> {
    put_known(
        objects,
        kind,
        payload,
        similar,
        ObjectId::hash(kind, payload),
    )
}

/// [`put_hinted`] for a payload whose id the caller computed already, as
/// `ObjectId::hash(kind, payload)`: the id the store gives back is checked
/// against it.
pub(crate) fn put_known<O: Objects>(
    objects: &O,
    kind: Kind,
    payload: &[u8],
    similar: Option<&ObjectId>,
    expected: ObjectId,
) -> Result<ObjectId, Error> {
    let id = match similar {
        Some(similar) => objects.put_similar(kind, payload, similar),
        None => objects.put(kind, payload),
    }
    .map_err(Error::objects)?;
    if id != expected {
        return Err(Error::BadBraidObject {
            id,
            detail: format!("the store gave this id to the {kind} {expected}"),
        });
    }
    Ok(id)
}

/// Directories, by id, so that reading one again costs a lookup.
///
/// What [`tree_entries`] gives for an id depends on nothing but the id: each
/// node it reads is checked to hash to its id, every name is checked, and the
/// nodes must be the canonical split of the entries. Keeping what it gave
/// changes only the cost of reading a directory again.
///
/// A one-thread import keeps only what it read back from the store, so its
/// checks, which read through this cache, still read what was stored, once.
/// A parallel import also keeps the directories it converted and has not
/// stored yet, as it built them ([`hold`](Self::hold)), since they cannot be
/// read from the store until their batch is stored; its checks read the
/// store directly, never through this cache.
///
/// Memory is bounded: once the directories used since the last drop hold
/// [`TREE_CACHE_ENTRIES`] entries, the ones not used since are dropped.
/// Directories held are never dropped until they are let go.
#[derive(Default)]
pub(crate) struct TreeCache {
    inner: Mutex<Generations>,
}

#[derive(Default)]
struct Generations {
    /// Directories converted and not stored yet.
    held: HashMap<ObjectId, Arc<[Entry]>>,
    /// Directories read or used since the last drop.
    recent: HashMap<ObjectId, Arc<[Entry]>>,
    /// How many entries they hold, counting an empty one as one.
    recent_entries: usize,
    /// Directories read or used before the last drop: dropped at the next.
    older: HashMap<ObjectId, Arc<[Entry]>>,
}

impl TreeCache {
    /// Every entry of the directory `root`: the ones kept, or
    /// [`tree_entries`].
    pub(crate) fn entries<O: Objects>(
        &self,
        objects: &O,
        root: ObjectId,
    ) -> Result<Arc<[Entry]>, Error> {
        if let Some(found) = self.lock().get(&root) {
            return Ok(found);
        }
        let read: Arc<[Entry]> = tree_entries(objects, root)?.into();
        self.lock().insert(root, Arc::clone(&read));
        Ok(read)
    }

    /// Keeps the entries of a directory that is not stored yet, until
    /// [`let_go`](Self::let_go).
    pub(crate) fn hold(&self, id: ObjectId, entries: Arc<[Entry]>) {
        self.lock().held.insert(id, entries);
    }

    /// The directories `ids` are stored now: they are kept like any other,
    /// and may be dropped.
    pub(crate) fn let_go(&self, ids: &[ObjectId]) {
        let mut inner = self.lock();
        for id in ids {
            if let Some(entries) = inner.held.remove(id) {
                inner.insert(*id, entries);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Generations> {
        // Every operation is a lookup or an insert: a panic cannot leave the
        // maps half-written.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Generations {
    fn get(&mut self, id: &ObjectId) -> Option<Arc<[Entry]>> {
        if let Some(found) = self.held.get(id).or_else(|| self.recent.get(id)) {
            return Some(Arc::clone(found));
        }
        let found = self.older.remove(id)?;
        self.insert(*id, Arc::clone(&found));
        Some(found)
    }

    fn insert(&mut self, id: ObjectId, entries: Arc<[Entry]>) {
        if self.recent_entries >= TREE_CACHE_ENTRIES {
            self.older = std::mem::take(&mut self.recent);
            self.recent_entries = 0;
        }
        self.recent_entries += entries.len().max(1);
        self.recent.insert(id, entries);
    }
}

/// Every entry of a directory, whether it is one flat node or split.
pub(crate) fn tree_entries<O: Objects>(objects: &O, root: ObjectId) -> Result<Vec<Entry>, Error> {
    let mut failure = None;
    let entries = read_directory(root, |id| match get(objects, id, Kind::Tree) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            failure.get_or_insert(err);
            None
        }
    });
    if let Some(err) = failure {
        return Err(err);
    }
    entries.map_err(|err| Error::BadBraidObject {
        id: root,
        detail: err.to_string(),
    })
}

/// A decoded changeset.
pub(crate) fn changeset<O: Objects>(objects: &O, id: &ObjectId) -> Result<Changeset, Error> {
    let bytes = get(objects, id, Kind::Changeset)?;
    Changeset::decode(&bytes).map_err(|err| Error::BadBraidObject {
        id: *id,
        detail: err.to_string(),
    })
}
