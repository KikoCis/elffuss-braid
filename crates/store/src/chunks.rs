// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Chunked blobs (codec 4): blobs of 4 MiB or more, cut into chunks that are
//! stored as blobs of their own.
//!
//! Cut points come from FastCDC (the 2020 algorithm, normalisation level 1,
//! the standard gear table) and depend only on the content, so a few bytes
//! changed in the middle of a large file change one or two chunks, and every
//! other chunk is shared with the previous version, or with any file that
//! holds the same bytes.
//!
//! | Blob size | Smallest chunk | Average | Largest |
//! |---|---|---|---|
//! | 4 MiB to 256 MiB | 16 KiB | 64 KiB | 256 KiB |
//! | 256 MiB or more | 256 KiB | 1 MiB | 4 MiB |
//!
//! The record of a chunked blob holds its chunk list:
//!
//! ```text
//! uv n | n × ([32] chunk id | uv chunk size)
//! ```
//!
//! The chunks, in order, make up the blob: their sizes add up to the
//! record's raw length, and the blob's id is the hash of the whole content,
//! checked on every read. A chunk is a blob whose id is the hash of its own
//! bytes, stored whole: raw when a sample of it does not compress below 0.9
//! of its size, else compressed when that is smaller.

use braid_format::codec::{Reader, put_id, put_uv};
use braid_format::{Kind, ObjectId};

use crate::record::{self, Encoded, ZSTD_LEVEL};

/// Blobs of this size or more are stored in chunks.
pub const CHUNKED_MIN: u64 = 4 << 20;

/// Blobs of this size or more are cut into larger chunks.
const LARGE_MIN: u64 = 256 << 20;

/// Bytes of a chunk compressed to tell whether the rest is worth trying.
const SAMPLE: usize = 64 << 10;

/// The FastCDC sizes for one blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Params {
    /// No chunk is smaller, except the last one.
    pub min: usize,
    /// The size chunks tend to.
    pub avg: usize,
    /// No chunk is larger.
    pub max: usize,
}

/// Chunks of blobs under [`LARGE_MIN`].
const SMALL: Params = Params {
    min: 16 << 10,
    avg: 64 << 10,
    max: 256 << 10,
};

/// Chunks of blobs of [`LARGE_MIN`] or more.
const LARGE: Params = Params {
    min: 256 << 10,
    avg: 1 << 20,
    max: 4 << 20,
};

/// The chunk sizes for a blob of `size` bytes.
pub(crate) fn params(size: u64) -> Params {
    if size >= LARGE_MIN { LARGE } else { SMALL }
}

/// Cuts a whole blob into chunks: the byte ranges, in order.
pub(crate) fn cut(blob: &[u8]) -> impl Iterator<Item = &[u8]> {
    let params = params(blob.len() as u64);
    fastcdc::v2020::FastCDC::new(blob, params.min, params.avg, params.max)
        .map(move |chunk| &blob[chunk.offset..chunk.offset + chunk.length])
}

/// Cuts a stream into chunks, sized for a blob of `size` bytes.
pub(crate) fn cut_stream<R: std::io::Read>(reader: R, size: u64) -> fastcdc::v2020::StreamCDC<R> {
    let params = params(size);
    fastcdc::v2020::StreamCDC::new(reader, params.min, params.avg, params.max)
}

/// One chunk of a chunked blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChunkRef {
    /// The chunk's id: the hash of its bytes as a blob.
    pub id: ObjectId,
    /// Its size in bytes.
    pub size: u64,
}

/// Encodes a chunk list.
pub(crate) fn encode_list(chunks: &[ChunkRef]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + chunks.len() * 36);
    put_uv(&mut out, chunks.len() as u64);
    for chunk in chunks {
        put_id(&mut out, &chunk.id);
        put_uv(&mut out, chunk.size);
    }
    out
}

/// Decodes the chunk list of a blob of `raw_len` bytes, checking that its
/// sizes add up.
pub(crate) fn decode_list(stored: &[u8], raw_len: u64) -> Result<Vec<ChunkRef>, &'static str> {
    let bad = |_| "bad chunk list";
    let mut reader = Reader::new(stored);
    // An entry takes at least 33 bytes.
    let n = reader.count(33).map_err(bad)?;
    if n == 0 {
        return Err("empty chunk list");
    }
    let mut chunks = Vec::with_capacity(n);
    let mut total = 0u64;
    for _ in 0..n {
        let id = reader.id().map_err(bad)?;
        let size = reader.uv().map_err(bad)?;
        if size == 0 {
            return Err("empty chunk in a chunk list");
        }
        total = total.checked_add(size).ok_or("chunk sizes overflow")?;
        chunks.push(ChunkRef { id, size });
    }
    reader.finish().map_err(bad)?;
    if total != raw_len {
        return Err("chunk sizes do not add up to the blob's size");
    }
    Ok(chunks)
}

/// Whether a chunk is worth compressing: its first 64 KiB must compress
/// below 0.9 of their size. Returns the compressed chunk when the sample was
/// the whole chunk.
fn sample(chunk: &[u8]) -> (bool, Option<Vec<u8>>) {
    let part = &chunk[..chunk.len().min(SAMPLE)];
    match zstd::bulk::compress(part, ZSTD_LEVEL) {
        Ok(compressed) if compressed.len() * 10 < part.len() * 9 => {
            let whole = (part.len() == chunk.len()).then_some(compressed);
            (true, whole)
        }
        _ => (false, None),
    }
}

/// The record of a chunk: raw when a sample of it does not compress well
/// enough, else whichever of raw and compressed is smaller.
pub(crate) fn encode_chunk(id: ObjectId, chunk: &[u8]) -> Result<Encoded, u64> {
    match sample(chunk) {
        (false, _) => record::encode_raw(Kind::Blob, id, chunk),
        (true, Some(compressed)) => record::assemble(
            Kind::Blob,
            id,
            record::CODEC_ZSTD,
            chunk.len() as u64,
            None,
            &compressed,
        ),
        (true, None) => record::encode(Kind::Blob, id, chunk),
    }
}

/// The record of a chunked blob.
pub(crate) fn encode_chunked(id: ObjectId, size: u64, chunks: &[ChunkRef]) -> Result<Encoded, u64> {
    record::assemble(
        Kind::Blob,
        id,
        record::CODEC_CHUNKED,
        size,
        None,
        &encode_list(chunks),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{CODEC_CHUNKED, CODEC_RAW, CODEC_ZSTD, check};

    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn sizes_follow_the_blob() {
        assert_eq!(params(CHUNKED_MIN), SMALL);
        assert_eq!(params(LARGE_MIN - 1), SMALL);
        assert_eq!(params(LARGE_MIN), LARGE);
        let blob = noise(3, 5 << 20);
        let chunks: Vec<&[u8]> = cut(&blob).collect();
        assert_eq!(chunks.concat(), blob);
        assert!(chunks.len() > 20, "{} chunks", chunks.len());
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(
                (SMALL.min..=SMALL.max).contains(&chunk.len()),
                "{}",
                chunk.len()
            );
        }
    }

    #[test]
    fn streaming_cuts_the_same_chunks() {
        let blob = noise(4, (6 << 20) + 12_345);
        let whole: Vec<Vec<u8>> = cut(&blob).map(<[u8]>::to_vec).collect();
        let streamed: Vec<Vec<u8>> = cut_stream(&blob[..], blob.len() as u64)
            .map(|chunk| chunk.unwrap().data)
            .collect();
        assert_eq!(whole, streamed);
    }

    #[test]
    fn chunk_lists_round_trip_and_are_checked() {
        let chunks: Vec<ChunkRef> = (0..300u64)
            .map(|i| ChunkRef {
                id: ObjectId::hash(Kind::Blob, &i.to_le_bytes()),
                size: 1000 + i * 77,
            })
            .collect();
        let total: u64 = chunks.iter().map(|c| c.size).sum();
        let list = encode_list(&chunks);
        assert_eq!(decode_list(&list, total).unwrap(), chunks);
        assert!(decode_list(&list, total + 1).is_err());
        assert!(decode_list(&list[..list.len() - 1], total).is_err());
        let mut longer = list.clone();
        longer.push(0);
        assert!(decode_list(&longer, total).is_err());
        assert_eq!(decode_list(&[0], 0), Err("empty chunk list"));
        // A count larger than the bytes could hold is refused up front.
        assert!(decode_list(&[0xff, 0xff, 0x03], 1).is_err());
    }

    #[test]
    fn chunks_that_do_not_compress_stay_raw() {
        let random = noise(5, 100_000);
        let id = ObjectId::hash(Kind::Blob, &random);
        assert_eq!(encode_chunk(id, &random).unwrap().header.codec, CODEC_RAW);
        for len in [1000, 50_000, 200_000] {
            let text = b"0123456789 compressible text\n".repeat(len / 29);
            let id = ObjectId::hash(Kind::Blob, &text);
            let encoded = encode_chunk(id, &text).unwrap();
            assert_eq!(encoded.header.codec, CODEC_ZSTD, "{len}");
            let checked = check(&encoded.bytes).unwrap();
            assert_eq!(
                record::decode(&checked.header, checked.stored).unwrap(),
                text
            );
        }
    }

    #[test]
    fn a_chunked_record_holds_the_list() {
        let chunks = [
            ChunkRef {
                id: ObjectId::hash(Kind::Blob, b"a"),
                size: 3,
            },
            ChunkRef {
                id: ObjectId::hash(Kind::Blob, b"b"),
                size: 4,
            },
        ];
        let id = ObjectId::hash(Kind::Blob, b"whole");
        let encoded = encode_chunked(id, 7, &chunks).unwrap();
        assert_eq!(encoded.header.codec, CODEC_CHUNKED);
        assert_eq!(encoded.header.raw_len, 7);
        let checked = check(&encoded.bytes).unwrap();
        assert_eq!(checked.stored, encode_list(&chunks));
        assert_eq!(decode_list(checked.stored, 7).unwrap(), chunks);
    }
}
