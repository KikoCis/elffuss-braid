// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Derived data: values computed from changesets, stored as derived objects
//! keyed by their inputs, and always recomputable.
//!
//! A derived value is what a named function, at some version, gives for some
//! inputs. Its key is
//!
//! ```text
//! BLAKE3-derive-key("elffuss-braid 2026 derived v1",
//!     bytes function | uv version | uv n | n × [32] input id | bytes argument)
//! ```
//!
//! with the codec of the format (`uv` and `bytes`); the argument is empty for
//! functions of ids alone. The value is stored as a derived object, a
//! *record*, whose payload is the key followed by the value's encoding:
//! `[32] key | value`. Each function versions its encoding on its own: a new
//! encoding is a new version, so a new key.
//!
//! A record's id depends on its value, so a [`DerivedIndex`] maps keys to
//! record ids. The index is only a cache: a record is used when its payload
//! starts with the key and its value decodes, and a value whose record is
//! missing, damaged or unindexed is computed again.
//!
//! Every value is a deterministic function of its inputs. Two processes that
//! compute the same value store the same bytes under the same id, storing an
//! object twice does nothing, and recording a key twice in the index does
//! nothing either: computing a missing value is safe while other processes do
//! the same, and the order in which values are computed never changes them.

use core::convert::Infallible;
use core::fmt;
use std::collections::HashMap;
use std::sync::Mutex;

use braid_format::codec::{put_bytes, put_id, put_uv};
use braid_format::{Kind, ObjectId};

/// The key of a derived value: a hash of the function, its version and its
/// inputs, `BLAKE3-derive-key("elffuss-braid 2026 derived v1", bytes function
/// | uv version | uv n | n × [32] input id | bytes argument)` with the codec
/// of the format (section 8 of the format spec).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DerivedKey([u8; 32]);

impl DerivedKey {
    /// Length of a key in bytes.
    pub const LEN: usize = 32;

    /// The key of `function` at `version` over `inputs` and `argument`.
    pub fn new(function: &str, version: u64, inputs: &[ObjectId], argument: &[u8]) -> Self {
        let mut preimage =
            Vec::with_capacity(function.len() + inputs.len() * ObjectId::LEN + argument.len() + 16);
        put_bytes(&mut preimage, function.as_bytes());
        put_uv(&mut preimage, version);
        put_uv(&mut preimage, inputs.len() as u64);
        for input in inputs {
            put_id(&mut preimage, input);
        }
        put_bytes(&mut preimage, argument);
        DerivedKey(*ObjectId::hash(Kind::Derived, &preimage).as_bytes())
    }

    /// Wraps raw key bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        DerivedKey(bytes)
    }

    /// The raw key bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for DerivedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&ObjectId::from_bytes(self.0), f)
    }
}

impl fmt::Debug for DerivedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DerivedKey({self})")
    }
}

/// Where the records of derived values are found from their keys.
///
/// An index is a cache: it may lack keys, and what it says is checked
/// against the record. Implementations must make the records an entry names
/// durable before the entry, so that an index never outlives its records.
pub trait DerivedIndex {
    /// Why a lookup or a write failed.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The id of the record of `key`, if the index knows one. When a key was
    /// recorded more than once, the last entry counts.
    fn find(&self, key: &DerivedKey) -> Result<Option<ObjectId>, Self::Error>;

    /// Records where the records of these keys are, in order. Recording an
    /// entry that is already there does nothing.
    fn record(&self, entries: &[(DerivedKey, ObjectId)]) -> Result<(), Self::Error>;
}

impl<T: DerivedIndex + ?Sized> DerivedIndex for &T {
    type Error = T::Error;

    fn find(&self, key: &DerivedKey) -> Result<Option<ObjectId>, Self::Error> {
        (**self).find(key)
    }

    fn record(&self, entries: &[(DerivedKey, ObjectId)]) -> Result<(), Self::Error> {
        (**self).record(entries)
    }
}

/// A derived index kept in memory, for tests and short-lived work.
#[derive(Debug, Default)]
pub struct MemoryIndex {
    map: Mutex<HashMap<DerivedKey, ObjectId>>,
}

impl MemoryIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of keys recorded.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no key is recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every entry, sorted by key.
    pub fn entries(&self) -> Vec<(DerivedKey, ObjectId)> {
        let mut entries: Vec<_> = self.lock().iter().map(|(k, v)| (*k, *v)).collect();
        entries.sort();
        entries
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<DerivedKey, ObjectId>> {
        // Every operation is a single insert or lookup: a panic cannot leave
        // the map half-written.
        self.map
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl DerivedIndex for MemoryIndex {
    type Error = Infallible;

    fn find(&self, key: &DerivedKey) -> Result<Option<ObjectId>, Self::Error> {
        Ok(self.lock().get(key).copied())
    }

    fn record(&self, entries: &[(DerivedKey, ObjectId)]) -> Result<(), Self::Error> {
        let mut map = self.lock();
        for (key, id) in entries {
            map.insert(*key, *id);
        }
        Ok(())
    }
}

/// The payload of the record of `key` holding `value`.
pub(crate) fn encode_record(key: &DerivedKey, value: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(DerivedKey::LEN + value.len());
    payload.extend_from_slice(key.as_bytes());
    payload.extend_from_slice(value);
    payload
}

/// The value in a record's payload, if the record is the one of `key`.
pub(crate) fn record_value<'p>(key: &DerivedKey, payload: &'p [u8]) -> Option<&'p [u8]> {
    payload.strip_prefix(key.as_bytes().as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_hash_the_documented_preimage() {
        let input = ObjectId::hash(Kind::Changeset, b"c");
        let key = DerivedKey::new("graph", 1, &[input], b"");
        let mut preimage = vec![5];
        preimage.extend_from_slice(b"graph");
        preimage.extend_from_slice(&[1, 1]);
        preimage.extend_from_slice(input.as_bytes());
        preimage.push(0);
        assert_eq!(
            key.as_bytes(),
            blake3::derive_key("elffuss-braid 2026 derived v1", &preimage).as_slice()
        );
        assert_eq!(
            key.to_string(),
            ObjectId::from_bytes(*key.as_bytes()).to_hex()
        );
    }

    #[test]
    fn every_part_of_the_preimage_counts() {
        let a = ObjectId::hash(Kind::Changeset, b"a");
        let b = ObjectId::hash(Kind::Changeset, b"b");
        let keys = [
            DerivedKey::new("graph", 1, &[a], b""),
            DerivedKey::new("graph", 2, &[a], b""),
            DerivedKey::new("paths", 1, &[a], b""),
            DerivedKey::new("graph", 1, &[b], b""),
            DerivedKey::new("graph", 1, &[a, b], b""),
            DerivedKey::new("graph", 1, &[a], b"x"),
            // The lengths keep the parts apart.
            DerivedKey::new("grap", 1, &[a], b"h"),
        ];
        for (i, x) in keys.iter().enumerate() {
            for y in &keys[i + 1..] {
                assert_ne!(x, y);
            }
        }
    }

    #[test]
    fn records_start_with_their_key() {
        let a = DerivedKey::new("f", 1, &[], b"a");
        let b = DerivedKey::new("f", 1, &[], b"b");
        let payload = encode_record(&a, b"value");
        assert_eq!(record_value(&a, &payload), Some(&b"value"[..]));
        assert_eq!(record_value(&b, &payload), None);
        assert_eq!(record_value(&a, &payload[..10]), None);
        assert_eq!(record_value(&a, a.as_bytes()), Some(&b""[..]));
    }

    #[test]
    fn memory_index() {
        let index = MemoryIndex::new();
        assert!(index.is_empty());
        let key = DerivedKey::new("f", 1, &[], b"");
        let one = ObjectId::hash(Kind::Derived, b"one");
        let two = ObjectId::hash(Kind::Derived, b"two");
        assert_eq!(index.find(&key).unwrap(), None);
        index.record(&[(key, one), (key, one)]).unwrap();
        assert_eq!(index.find(&key).unwrap(), Some(one));
        // The last entry for a key counts.
        <&MemoryIndex as DerivedIndex>::record(&&index, &[(key, two)]).unwrap();
        assert_eq!(index.find(&key).unwrap(), Some(two));
        assert_eq!(index.len(), 1);
        assert_eq!(index.entries(), [(key, two)]);
    }
}
