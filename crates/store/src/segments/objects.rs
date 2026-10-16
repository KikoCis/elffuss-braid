// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Objects in every stored form: whole, as deltas, or in chunks.
//!
//! - **Writes.** A blob of 4 MiB or more is cut into chunks, each stored as
//!   a blob of its own, then its chunk list. Anything else is stored whole,
//!   unless the writer names a similar object: a blob under 4 MiB, a tree
//!   or a derived object is then stored as a delta against that object, or
//!   against its base if it is a delta itself, when the delta takes at most
//!   a quarter of the bytes.
//! - **Reads.** A delta needs its base, stored whole; a chunked blob needs
//!   its chunks, stored whole or as deltas. Whatever the form, what comes
//!   back hashes to the id asked for, or the read fails.
//! - **Bases.** Each handle keeps the payloads of the bases it read last, so
//!   that reading the versions of one file, or writing them, decodes their
//!   base once.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, PoisonError};

use braid_format::{IdHasher, Kind, ObjectId};

use super::{Location, Record, Segments, Wanted, too_large};
use crate::chunks::{self, CHUNKED_MIN, ChunkRef};
use crate::delta;
use crate::error::Error;
use crate::record::{self, CODEC_CHUNKED, CODEC_DELTA};

/// How many base payloads a handle keeps.
const CACHED_BASES: usize = 64;

/// How many bytes the base payloads a handle keeps may take together.
const CACHED_BASE_BYTES: usize = 16 << 20;

/// The largest base a delta is made against: larger objects are not read
/// whole into memory to save a few bytes.
const MAX_BASE: u64 = 64 << 20;

/// The payloads of recently used delta bases, most recent first.
#[derive(Debug, Default)]
pub(crate) struct BaseCache {
    inner: Mutex<Bases>,
}

#[derive(Debug, Default)]
struct Bases {
    entries: VecDeque<(ObjectId, Arc<Vec<u8>>)>,
    bytes: usize,
}

impl BaseCache {
    /// The payload of `id`, if it is kept.
    pub(crate) fn get(&self, id: &ObjectId) -> Option<Arc<Vec<u8>>> {
        let mut bases = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let at = bases.entries.iter().position(|(cached, _)| cached == id)?;
        let entry = bases.entries.remove(at)?;
        let payload = Arc::clone(&entry.1);
        bases.entries.push_front(entry);
        Some(payload)
    }

    /// Keeps the payload of `id`, dropping the least recently used ones
    /// past the limits.
    pub(crate) fn insert(&self, id: ObjectId, payload: Arc<Vec<u8>>) {
        if payload.len() > CACHED_BASE_BYTES / 4 {
            return;
        }
        let mut bases = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if bases.entries.iter().any(|(cached, _)| *cached == id) {
            return;
        }
        bases.bytes += payload.len();
        bases.entries.push_front((id, payload));
        while bases.entries.len() > CACHED_BASES || bases.bytes > CACHED_BASE_BYTES {
            match bases.entries.pop_back() {
                Some((_, evicted)) => bases.bytes -= evicted.len(),
                None => break,
            }
        }
    }
}

impl Segments {
    /// Stores an object and returns its id: a blob of 4 MiB or more in
    /// chunks, anything else whole. Nothing is flushed: call
    /// [`Segments::sync`] before anything durable refers to the object.
    pub fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Error> {
        let id = ObjectId::hash(kind, payload);
        if kind == Kind::Blob && payload.len() as u64 >= CHUNKED_MIN {
            if !self.stored(&id, Wanted::Any)? {
                let mut chunks = Vec::new();
                for chunk in chunks::cut(payload) {
                    chunks.push(self.put_chunk(chunk)?);
                }
                self.finish_chunked(id, payload.len() as u64, &chunks)?;
            }
        } else {
            self.store_record(&id, Wanted::Any, || {
                record::encode(kind, id, payload).map_err(too_large)
            })?;
        }
        Ok(id)
    }

    /// Stores an object that is probably close to `similar`. A blob under
    /// 4 MiB, a tree or a derived object becomes a delta when that takes at
    /// most a quarter of the bytes of storing it on its own; anything else
    /// is stored as [`Segments::put`] does. The hint never makes a write
    /// fail: when the similar object is missing, damaged or unusable as a
    /// base, the object is stored on its own.
    pub fn put_similar(
        &self,
        kind: Kind,
        payload: &[u8],
        similar: &ObjectId,
    ) -> Result<ObjectId, Error> {
        if !delta::eligible(kind, payload.len()) {
            return self.put(kind, payload);
        }
        let id = ObjectId::hash(kind, payload);
        if self.stored(&id, Wanted::Any)? {
            return Ok(id);
        }
        let (codec, alone) = record::standalone(payload);
        let delta = match self.delta_base(similar) {
            Some((base_id, base)) => delta::encode(kind, id, payload, &base_id, &base, alone.len()),
            None => None,
        };
        let encoded = match delta {
            Some(delta) => delta,
            None => record::assemble(kind, id, codec, payload.len() as u64, None, &alone)
                .map_err(too_large)?,
        };
        self.store_record(&id, Wanted::Any, || Ok(encoded))?;
        Ok(id)
    }

    /// Stores a blob read from `reader` without holding it in memory
    /// whole, and returns its id and size. Up to 4 MiB it is stored like
    /// [`Segments::put`]; past that, it is cut into chunks as it streams in,
    /// with chunk sizes for a blob of `size_hint` bytes, the size the caller
    /// expects. A stream that turns out longer or shorter is still stored
    /// exactly as read; only how it is cut depends on the hint.
    pub fn put_blob_from(
        &self,
        reader: &mut dyn Read,
        size_hint: Option<u64>,
    ) -> Result<(ObjectId, u64), Error> {
        let read_error = |source| Error::Stream {
            action: "read",
            source,
        };
        let mut head = Vec::new();
        (&mut *reader)
            .take(CHUNKED_MIN)
            .read_to_end(&mut head)
            .map_err(read_error)?;
        if (head.len() as u64) < CHUNKED_MIN {
            let id = self.put(Kind::Blob, &head)?;
            return Ok((id, head.len() as u64));
        }
        let size = size_hint.unwrap_or(0).max(CHUNKED_MIN);
        let mut hasher = IdHasher::new(Kind::Blob);
        let mut chunks = Vec::new();
        let mut total = 0u64;
        for chunk in chunks::cut_stream(io::Cursor::new(head).chain(reader), size) {
            let chunk = chunk.map_err(|err| read_error(err.into()))?;
            hasher.update(&chunk.data);
            total += chunk.data.len() as u64;
            chunks.push(self.put_chunk(&chunk.data)?);
        }
        let id = hasher.finalize();
        self.finish_chunked(id, total, &chunks)?;
        Ok((id, total))
    }

    /// Stores one chunk of a chunked blob, whole: a chunk list never names a
    /// chunked blob.
    fn put_chunk(&self, chunk: &[u8]) -> Result<ChunkRef, Error> {
        let id = ObjectId::hash(Kind::Blob, chunk);
        self.store_record(&id, Wanted::NotChunked, || {
            chunks::encode_chunk(id, chunk).map_err(too_large)
        })?;
        Ok(ChunkRef {
            id,
            size: chunk.len() as u64,
        })
    }

    /// Stores the chunk list of the blob `id`, whose chunks are stored.
    fn finish_chunked(&self, id: ObjectId, size: u64, chunks: &[ChunkRef]) -> Result<(), Error> {
        // A blob of one chunk is that chunk, stored whole already.
        if chunks.len() == 1 {
            return Ok(());
        }
        self.store_record(&id, Wanted::Any, || {
            chunks::encode_chunked(id, size, chunks).map_err(too_large)
        })
    }

    /// The base to store a delta against, given the object a writer says is
    /// similar, with its payload: that object if it is stored whole, or its
    /// base if it is a delta. `None` when there is none to use.
    fn delta_base(&self, similar: &ObjectId) -> Option<(ObjectId, Arc<Vec<u8>>)> {
        let location = self.location(similar)?;
        let (base, location) = match location.entry.codec {
            codec if record::is_whole(codec) => (*similar, location),
            CODEC_DELTA => {
                let base = self.reference(&location).ok()??;
                (base, self.location(&base)?)
            }
            _ => return None,
        };
        if location.entry.raw_len > MAX_BASE {
            return None;
        }
        // The base must stay readable for as long as the delta: whole, and
        // flushed by this handle's next sync if its writer has not yet.
        if !self.rely_on_base(&location).ok()? {
            return None;
        }
        let payload = self.base_payload(&base, &location).ok()?;
        Some((base, payload))
    }

    /// The payload of the base `id`, stored whole at `location`.
    fn base_payload(&self, id: &ObjectId, location: &Location) -> Result<Arc<Vec<u8>>, Error> {
        if let Some(payload) = self.bases.get(id) {
            return Ok(payload);
        }
        let base = self.read_record(location)?;
        let payload = record::decode(&base.header, base.stored()).map_err(|d| base.corrupt(d))?;
        let payload = Arc::new(payload);
        self.bases.insert(*id, Arc::clone(&payload));
        Ok(payload)
    }

    /// Reads an object, checking every record it takes and its id.
    pub fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Error> {
        let Some(location) = self.find(id)? else {
            return Ok(None);
        };
        let payload = if location.entry.codec == CODEC_CHUNKED {
            let capacity = usize::try_from(location.entry.raw_len.min(1 << 24)).unwrap_or(0);
            let mut payload = Vec::with_capacity(capacity);
            self.read_chunked(&location, &mut |bytes| {
                payload.extend_from_slice(bytes);
                Ok(())
            })?;
            payload
        } else {
            self.read_single(&location)?
        };
        Ok(Some((location.entry.kind, payload)))
    }

    /// Writes an object's payload to `out`, a chunk at a time for a chunked
    /// blob, so that a blob of any size can be read with little memory.
    /// Returns its kind and size, or `None` if it is not here.
    ///
    /// Every chunk is checked before it is written, and the whole blob's id
    /// after the last one: on an error, discard what `out` received.
    pub fn read_to(
        &self,
        id: &ObjectId,
        out: &mut dyn Write,
    ) -> Result<Option<(Kind, u64)>, Error> {
        let Some(location) = self.find(id)? else {
            return Ok(None);
        };
        let mut write = |bytes: &[u8]| {
            out.write_all(bytes).map_err(|source| Error::Stream {
                action: "write",
                source,
            })
        };
        if location.entry.codec == CODEC_CHUNKED {
            self.read_chunked(&location, &mut write)?;
        } else {
            write(&self.read_single(&location)?)?;
        }
        Ok(Some((location.entry.kind, location.entry.raw_len)))
    }

    /// Reads an object stored in one record: whole, or as a delta.
    fn read_single(&self, location: &Location) -> Result<Vec<u8>, Error> {
        let found = self.read_record(location)?;
        match (found.header.codec, found.reference) {
            (CODEC_DELTA, Some(base)) => {
                let base = self.base_of(&found, &base)?;
                delta::decode(&found.header, found.stored(), &base).map_err(|d| found.corrupt(d))
            }
            _ => record::decode(&found.header, found.stored()).map_err(|d| found.corrupt(d)),
        }
    }

    /// The payload of the base a delta names. A handle that only knows a
    /// copy of the base that is not whole looks at the directory again once,
    /// since the copy the delta was made against may be new to it.
    fn base_of(&self, delta: &Record, base: &ObjectId) -> Result<Arc<Vec<u8>>, Error> {
        if let Some(payload) = self.bases.get(base) {
            return Ok(payload);
        }
        let mut location = self.find(base)?;
        if location.is_some_and(|found| !record::is_whole(found.entry.codec)) {
            self.rescan()?;
            location = self.location(base);
        }
        let location = match location {
            None => return Err(delta.corrupt(format_args!("its base {base} is missing"))),
            Some(found) if !record::is_whole(found.entry.codec) => {
                return Err(delta.corrupt(format_args!("its base {base} is not stored whole")));
            }
            Some(found) => found,
        };
        self.base_payload(base, &location).map_err(|err| match err {
            Error::Corrupt { detail, .. } => delta.corrupt(format_args!("its base: {detail}")),
            other => other,
        })
    }

    /// Reads a chunked blob a chunk at a time, passing each chunk's bytes to
    /// `sink` in order, then checks that the whole hashes to the blob's id.
    fn read_chunked(
        &self,
        location: &Location,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let list = self.read_record(location)?;
        let chunks =
            chunks::decode_list(list.stored(), list.header.raw_len).map_err(|d| list.corrupt(d))?;
        let mut hasher = IdHasher::new(Kind::Blob);
        for chunk in &chunks {
            let bytes = self.read_chunk(&list, chunk)?;
            hasher.update(&bytes);
            sink(&bytes)?;
        }
        if hasher.finalize() != list.header.id {
            return Err(list.corrupt("content does not hash to its id"));
        }
        Ok(())
    }

    /// Reads one chunk of the chunked blob `list`.
    fn read_chunk(&self, list: &Record, chunk: &ChunkRef) -> Result<Vec<u8>, Error> {
        let mut location = self.find(&chunk.id)?;
        if location.is_some_and(|found| found.entry.codec == CODEC_CHUNKED) {
            self.rescan()?;
            location = self.location(&chunk.id);
        }
        let location = match location {
            None => {
                return Err(list.corrupt(format_args!("its chunk {} is missing", chunk.id)));
            }
            Some(found)
                if found.entry.codec == CODEC_CHUNKED
                    || found.entry.kind != Kind::Blob
                    || found.entry.raw_len != chunk.size =>
            {
                return Err(list.corrupt(format_args!(
                    "its chunk {} is not stored as a blob of {} bytes",
                    chunk.id, chunk.size
                )));
            }
            Some(found) => found,
        };
        self.read_single(&location).map_err(|err| match err {
            Error::Corrupt { detail, .. } => list.corrupt(format_args!("its chunk: {detail}")),
            other => other,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{CODEC_RAW, CODEC_ZSTD};
    use crate::{Options, STORAGE_VERSION};

    fn segments(dir: &std::path::Path) -> Segments {
        std::fs::create_dir_all(dir.join("seg")).unwrap();
        Segments::open(dir, &Options::default(), STORAGE_VERSION).unwrap()
    }

    /// Source-like text: lines that compress only a little on their own.
    fn text(seed: u64, lines: usize) -> Vec<u8> {
        let mut state = seed | 1;
        let mut out = Vec::new();
        for i in 0..lines {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(format!("{i}: {state:016x}\n").as_bytes());
        }
        out
    }

    fn edit(mut payload: Vec<u8>, at: usize, with: &[u8]) -> Vec<u8> {
        payload[at..at + with.len()].copy_from_slice(with);
        payload
    }

    fn codec(segments: &Segments, id: &ObjectId) -> u8 {
        segments.location(id).unwrap().entry.codec
    }

    fn base(segments: &Segments, id: &ObjectId) -> Option<ObjectId> {
        segments.reference(&segments.location(id).unwrap()).unwrap()
    }

    #[test]
    fn deltas_are_one_level_deep() {
        let dir = tempfile::tempdir().unwrap();
        let segments = segments(dir.path());
        let v1 = text(1, 500);
        let v2 = edit(v1.clone(), 100, b"two");
        let v3 = edit(v2.clone(), 2000, b"three");
        let id1 = segments.put(Kind::Blob, &v1).unwrap();
        let id2 = segments.put_similar(Kind::Blob, &v2, &id1).unwrap();
        let id3 = segments.put_similar(Kind::Blob, &v3, &id2).unwrap();
        assert_eq!(codec(&segments, &id1), CODEC_ZSTD);
        assert_eq!(codec(&segments, &id2), CODEC_DELTA);
        assert_eq!(codec(&segments, &id3), CODEC_DELTA);
        // v3 was said to be like v2, a delta: it is made against v2's base.
        assert_eq!(base(&segments, &id2), Some(id1));
        assert_eq!(base(&segments, &id3), Some(id1));
        for (id, payload) in [(id1, &v1), (id2, &v2), (id3, &v3)] {
            assert_eq!(
                segments.get(&id).unwrap(),
                Some((Kind::Blob, payload.clone()))
            );
        }

        // Trees too; other kinds, small payloads and large blobs never.
        let tree = segments.put(Kind::Tree, &v1).unwrap();
        let tree2 = segments.put_similar(Kind::Tree, &v2, &tree).unwrap();
        assert_eq!(codec(&segments, &tree2), CODEC_DELTA);
        let changeset = segments.put_similar(Kind::Changeset, &v2, &id1).unwrap();
        assert_ne!(codec(&segments, &changeset), CODEC_DELTA);
        let small = segments.put_similar(Kind::Blob, b"tiny", &id1).unwrap();
        assert_eq!(codec(&segments, &small), CODEC_RAW);
    }

    #[test]
    fn hints_that_cannot_serve_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let segments = segments(dir.path());
        let v1 = text(2, 400);
        let absent = ObjectId::hash(Kind::Blob, b"never stored");
        let id = segments.put_similar(Kind::Blob, &v1, &absent).unwrap();
        assert_eq!(codec(&segments, &id), CODEC_ZSTD);
        // Unrelated content: the delta would not take a quarter or less.
        let other = text(3, 400);
        let other_id = segments.put_similar(Kind::Blob, &other, &id).unwrap();
        assert_eq!(codec(&segments, &other_id), CODEC_ZSTD);
        // A chunked blob is never a base.
        let big = vec![7u8; CHUNKED_MIN as usize];
        let big_id = segments.put(Kind::Blob, &big).unwrap();
        assert_eq!(codec(&segments, &big_id), CODEC_CHUNKED);
        let near = edit(big[..1000].to_vec(), 10, b"x");
        let near_id = segments.put_similar(Kind::Blob, &near, &big_id).unwrap();
        assert_ne!(codec(&segments, &near_id), CODEC_DELTA);
        // Storing again is a no-op, hint or not.
        assert_eq!(
            segments.put_similar(Kind::Blob, &v1, &other_id).unwrap(),
            id
        );
        assert_eq!(codec(&segments, &id), CODEC_ZSTD);
    }

    #[test]
    fn a_copy_stored_whole_wins_over_a_delta() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = text(4, 300);
        let v2 = edit(v1.clone(), 50, b"changed");
        // Two handles, each unaware of the other's copy of v2.
        let a = segments(dir.path());
        let b = segments(dir.path());
        let id1 = a.put(Kind::Blob, &v1).unwrap();
        b.rescan().unwrap();
        let id2 = a.put_similar(Kind::Blob, &v2, &id1).unwrap();
        assert_eq!(codec(&a, &id2), CODEC_DELTA);
        let whole = b.put(Kind::Blob, &v2).unwrap();
        assert_eq!(whole, id2);
        a.sync().unwrap();
        b.sync().unwrap();
        // A third handle sees both copies and keeps the whole one, so that a
        // delta against v2 has a base stored whole.
        let c = segments(dir.path());
        assert_eq!(codec(&c, &id2), CODEC_ZSTD);
        let v3 = edit(v2.clone(), 900, b"third");
        let id3 = c.put_similar(Kind::Blob, &v3, &id2).unwrap();
        assert_eq!(base(&c, &id3), Some(id2));
        assert_eq!(c.get(&id3).unwrap(), Some((Kind::Blob, v3)));
    }

    #[test]
    fn the_base_cache_keeps_the_most_recent() {
        let cache = BaseCache::default();
        let id = |n: u32| ObjectId::hash(Kind::Blob, &n.to_le_bytes());
        for n in 0..(CACHED_BASES as u32 + 10) {
            cache.insert(id(n), Arc::new(vec![0; 10]));
            // Keep the first one in use.
            assert!(cache.get(&id(0)).is_some(), "{n}");
        }
        assert!(cache.get(&id(1)).is_none());
        assert!(cache.get(&id(CACHED_BASES as u32 + 9)).is_some());
        // Bytes are bounded too, and payloads that are too large stay out.
        cache.insert(id(1000), Arc::new(vec![0; CACHED_BASE_BYTES / 4 + 1]));
        assert!(cache.get(&id(1000)).is_none());
        for n in 2000..2010 {
            cache.insert(id(n), Arc::new(vec![0; CACHED_BASE_BYTES / 4]));
        }
        let bases = cache.inner.lock().unwrap();
        assert!(bases.bytes <= CACHED_BASE_BYTES);
        assert_eq!(
            bases.bytes,
            bases.entries.iter().map(|(_, p)| p.len()).sum::<usize>()
        );
    }
}
