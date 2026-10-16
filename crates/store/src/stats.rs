// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! What a store holds and how it is stored, from the indexes and journals
//! alone.

use std::fs;

use braid_format::Kind;

use crate::error::{Error, io_error};
use crate::record::{CODEC_CHUNKED, CODEC_DELTA, CODEC_DICTIONARY, CODEC_RAW, CODEC_ZSTD};
use crate::segments::list;
use crate::{Codec, Store};

/// The objects of one kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindStats {
    /// The kind.
    pub kind: Kind,
    /// Objects of this kind.
    pub objects: u64,
    /// Their payloads as stored, in bytes.
    pub stored_bytes: u64,
    /// Their payloads decoded, in bytes.
    pub raw_bytes: u64,
}

/// The objects stored with one codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecStats {
    /// The codec.
    pub codec: Codec,
    /// Objects stored this way.
    pub objects: u64,
    /// Their payloads as stored, in bytes: for chunked blobs, their chunk
    /// lists.
    pub stored_bytes: u64,
    /// Their payloads decoded, in bytes.
    pub raw_bytes: u64,
}

/// What a store holds: see [`Store::stats`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Distinct objects, chunks included: a chunk is a blob of its own.
    pub objects: u64,
    /// Objects of each kind, in code order.
    pub kinds: Vec<KindStats>,
    /// Objects of each codec, in code order.
    pub codecs: Vec<CodecStats>,
    /// Bytes of every object's payload as stored.
    pub stored_bytes: u64,
    /// Bytes of distinct content: every object's decoded size, except that
    /// a chunked blob counts only through its chunks, so that content shared
    /// by several versions counts once.
    pub raw_bytes: u64,
    /// Bytes of the objects' records, with their headers and padding.
    pub record_bytes: u64,
    /// Bytes of segment files.
    pub segment_bytes: u64,
    /// Bytes of indexes and journals.
    pub index_bytes: u64,
}

impl Store {
    /// What the store holds: objects by kind and by codec, with their sizes
    /// as stored and decoded, and the size of the files that hold them.
    ///
    /// It reads the directory again first, then counts one copy of each
    /// object from the indexes and journals: no payload is read, so the cost
    /// follows the number of objects, not their size. Copies of an object
    /// written twice by racing writers, and remains of writes cut short,
    /// count in the file sizes only.
    pub fn stats(&self) -> Result<Stats, Error> {
        self.segments.rescan()?;
        let mut kinds: Vec<KindStats> = Kind::ALL
            .iter()
            .map(|&kind| KindStats {
                kind,
                objects: 0,
                stored_bytes: 0,
                raw_bytes: 0,
            })
            .collect();
        let mut codecs: Vec<CodecStats> = Codec::ALL
            .iter()
            .map(|&codec| CodecStats {
                codec,
                objects: 0,
                stored_bytes: 0,
                raw_bytes: 0,
            })
            .collect();
        let mut stats = Stats {
            objects: 0,
            kinds: Vec::new(),
            codecs: Vec::new(),
            stored_bytes: 0,
            raw_bytes: 0,
            record_bytes: 0,
            segment_bytes: 0,
            index_bytes: 0,
        };
        for entry in self.segments.known_entries() {
            let stored = u64::from(entry.stored_len);
            stats.objects += 1;
            stats.stored_bytes += stored;
            stats.record_bytes += entry.header().record_len();
            if entry.codec != CODEC_CHUNKED {
                stats.raw_bytes += entry.raw_len;
            }
            if let Some(kind) = kinds.iter_mut().find(|k| k.kind == entry.kind) {
                kind.objects += 1;
                kind.stored_bytes += stored;
                kind.raw_bytes += entry.raw_len;
            }
            if let Some(codec) = codecs.iter_mut().find(|c| c.codec.code() == entry.codec) {
                codec.objects += 1;
                codec.stored_bytes += stored;
                codec.raw_bytes += entry.raw_len;
            }
        }
        stats.kinds = kinds;
        stats.codecs = codecs;
        let dir = self.segments.dir();
        for (name, files) in list(dir)? {
            for (extension, present) in [("seg", files.seg), ("jrn", files.jrn), ("idx", files.idx)]
            {
                if !present {
                    continue;
                }
                let path = self.segments.path(&name, extension);
                let len = match fs::metadata(&path) {
                    Ok(meta) => meta.len(),
                    // A journal removed by sealing since the listing.
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
                    Err(err) => return Err(io_error("read", &path)(err)),
                };
                if extension == "seg" {
                    stats.segment_bytes += len;
                } else {
                    stats.index_bytes += len;
                }
            }
        }
        Ok(stats)
    }
}

impl Codec {
    /// Every codec, in code order.
    pub const ALL: [Codec; 5] = [
        Codec::Raw,
        Codec::Zstd,
        Codec::Dictionary,
        Codec::Delta,
        Codec::Chunked,
    ];

    /// The number records store.
    pub const fn code(self) -> u8 {
        match self {
            Codec::Raw => CODEC_RAW,
            Codec::Zstd => CODEC_ZSTD,
            Codec::Dictionary => CODEC_DICTIONARY,
            Codec::Delta => CODEC_DELTA,
            Codec::Chunked => CODEC_CHUNKED,
        }
    }

    /// A short, stable name for reports and JSON.
    pub const fn name(self) -> &'static str {
        match self {
            Codec::Raw => "raw",
            Codec::Zstd => "zstd",
            Codec::Dictionary => "dictionary",
            Codec::Delta => "delta",
            Codec::Chunked => "chunked",
        }
    }
}
