// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! History queries answered with derived data, such as ancestry in
//! [`ancestry`](crate::ancestry).
//!
//! A [`History`] reads objects, finds derived records through a
//! [`DerivedIndex`], computes the values that are missing, stores them as
//! records and remembers every value it used. It records the new records in
//! the index by batches, and at the end when [`History::flush`] is called; a
//! value computed and never recorded is only computed again later.

use core::fmt;
use std::collections::HashMap;
use std::rc::Rc;

use braid_format::{Kind, ObjectId, Objects};

use crate::ancestry::GraphNode;
use crate::derived::{DerivedIndex, DerivedKey, encode_record, record_value};
use crate::error::{Error, store_error};

/// New records are recorded in the index once this many wait.
const RECORD_BATCH: usize = 4096;

/// History queries over the objects of `O`, with the derived data found
/// through `I`, computed when missing.
///
/// A `History` keeps every derived value it reads or computes in memory, so
/// a query never reads the same value twice; drop it to free them. Call
/// [`flush`](Self::flush) before dropping it to record the new values in the
/// index.
pub struct History<'o, O, I> {
    pub(crate) objects: &'o O,
    index: I,
    /// Commit graph data, by changeset.
    pub(crate) nodes: HashMap<ObjectId, Rc<GraphNode>>,
    /// Records stored and not recorded in the index yet.
    pending: Vec<(DerivedKey, ObjectId)>,
    /// Records this history computed and stored.
    computed: u64,
}

impl<O, I> fmt::Debug for History<'_, O, I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("History")
            .field("nodes", &self.nodes.len())
            .field("pending", &self.pending.len())
            .field("computed", &self.computed)
            .finish_non_exhaustive()
    }
}

impl<'o, O: Objects, I: DerivedIndex> History<'o, O, I> {
    /// A history over `objects`, finding and recording derived records
    /// through `index`.
    pub fn new(objects: &'o O, index: I) -> Self {
        History {
            objects,
            index,
            nodes: HashMap::new(),
            pending: Vec::new(),
            computed: 0,
        }
    }

    /// The objects this history reads.
    pub fn objects(&self) -> &'o O {
        self.objects
    }

    /// How many derived records this history computed and stored.
    pub fn computed(&self) -> u64 {
        self.computed
    }

    /// Records in the index every record stored since the last flush.
    pub fn flush(&mut self) -> Result<(), Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.index.record(&self.pending).map_err(store_error)?;
        self.pending.clear();
        Ok(())
    }

    /// The value of `key`, from its record, if the index knows the record
    /// and `decode` accepts its value. Anything else is a miss.
    pub(crate) fn load<T>(
        &mut self,
        key: &DerivedKey,
        decode: impl FnOnce(&[u8]) -> Option<T>,
    ) -> Result<Option<T>, Error> {
        let Some(record) = self.index.find(key).map_err(store_error)? else {
            return Ok(None);
        };
        let payload = match self.objects.get(&record).map_err(store_error)? {
            Some((Kind::Derived, payload)) if ObjectId::hash(Kind::Derived, &payload) == record => {
                payload
            }
            _ => return Ok(None),
        };
        Ok(record_value(key, &payload).and_then(decode))
    }

    /// Stores `value` as the record of `key`.
    pub(crate) fn save(&mut self, key: DerivedKey, value: &[u8]) -> Result<(), Error> {
        let record = self
            .objects
            .put(Kind::Derived, &encode_record(&key, value))
            .map_err(store_error)?;
        self.pending.push((key, record));
        self.computed += 1;
        if self.pending.len() >= RECORD_BATCH {
            self.flush()?;
        }
        Ok(())
    }
}
