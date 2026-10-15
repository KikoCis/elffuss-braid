// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Access to stored objects, independent of where they live: the local store,
//! a cache, a remote server, or memory.

use core::convert::Infallible;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::id::{Kind, ObjectId};

/// A place objects can be read from and written to.
pub trait Objects {
    /// Why an access failed: I/O, corruption, a lost connection.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The kind and payload of an object, or `None` if it is not here.
    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Self::Error>;

    /// Stores `payload` as an object of `kind` and returns its id. Storing an
    /// object that is already present does nothing.
    fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Self::Error>;

    /// Whether the object is here.
    fn contains(&self, id: &ObjectId) -> Result<bool, Self::Error> {
        Ok(self.get(id)?.is_some())
    }

    /// The kind and payload size of an object, or `None` if it is not here.
    /// Stores that know the size without reading the payload should override
    /// this, so that checks on large files stay cheap.
    fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, Self::Error> {
        Ok(self
            .get(id)?
            .map(|(kind, payload)| (kind, payload.len() as u64)))
    }
}

impl<T: Objects + ?Sized> Objects for &T {
    type Error = T::Error;

    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Self::Error> {
        (**self).get(id)
    }

    fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Self::Error> {
        (**self).put(kind, payload)
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, Self::Error> {
        (**self).contains(id)
    }

    fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, Self::Error> {
        (**self).stat(id)
    }
}

/// Objects kept in memory, for tests and short-lived work.
#[derive(Debug, Default)]
pub struct MemoryObjects {
    map: Mutex<HashMap<ObjectId, (Kind, Vec<u8>)>>,
}

impl MemoryObjects {
    /// An empty set of objects.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of objects held.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no object is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The ids of every object held, in no particular order.
    pub fn ids(&self) -> Vec<ObjectId> {
        self.lock().keys().copied().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ObjectId, (Kind, Vec<u8>)>> {
        // A panic while holding the lock cannot leave the map half-written:
        // every operation is a single insert or lookup.
        self.map
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Objects for MemoryObjects {
    type Error = Infallible;

    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Self::Error> {
        Ok(self.lock().get(id).cloned())
    }

    fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Self::Error> {
        let id = ObjectId::hash(kind, payload);
        self.lock()
            .entry(id)
            .or_insert_with(|| (kind, payload.to_vec()));
        Ok(id)
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, Self::Error> {
        Ok(self.lock().contains_key(id))
    }

    fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, Self::Error> {
        Ok(self
            .lock()
            .get(id)
            .map(|(kind, payload)| (*kind, payload.len() as u64)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_objects() {
        let objects = MemoryObjects::new();
        assert!(objects.is_empty());
        let id = objects.put(Kind::Blob, b"hello\n").unwrap();
        assert_eq!(id, ObjectId::hash(Kind::Blob, b"hello\n"));
        assert_eq!(objects.put(Kind::Blob, b"hello\n").unwrap(), id);
        assert_eq!(objects.len(), 1);
        assert_eq!(
            objects.get(&id).unwrap(),
            Some((Kind::Blob, b"hello\n".to_vec()))
        );
        assert!(objects.contains(&id).unwrap());
        assert_eq!(objects.stat(&id).unwrap(), Some((Kind::Blob, 6)));
        let other = ObjectId::hash(Kind::Tree, b"hello\n");
        assert_eq!(objects.get(&other).unwrap(), None);
        // Through a reference, too.
        let by_ref: &MemoryObjects = &objects;
        assert!(Objects::contains(&by_ref, &id).unwrap());
    }
}
