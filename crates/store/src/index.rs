// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Indexes of sealed segments (format spec, section 7).
//!
//! ```text
//! [8] magic "braididx" | u32 version = 1 | u32 0 | u64 count | u64 segment length
//! 256 × u32 fan-out: entry i is the number of ids whose first byte is at most i
//! count × [32] ids, strictly ascending
//! count × 32-byte locations, in the same order as the ids:
//!     u64 offset | u32 stored_len | u8 kind | u8 codec | u16 0
//!     u64 raw_len | u32 record crc | u32 0
//! [32] BLAKE3 hash of everything before it
//! ```
//!
//! An index is written once, to a temporary file that is flushed and renamed
//! into place, and never changes afterwards. The segment length is the size
//! of the segment when it was sealed. The binary fuse filter that the spec
//! plans for fast misses is not written by this version: indexes are read
//! into memory whole.

use crate::journal::Entry;
use crate::record::array;

const MAGIC: &[u8; 8] = b"braididx";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 32;
const FANOUT_LEN: usize = 256 * 4;
const LOCATION_LEN: usize = 32;
const FOOTER_LEN: usize = 32;

/// Encodes an index. `entries` must be sorted by id, without duplicates.
pub(crate) fn encode(entries: &[Entry], segment_len: u64) -> Vec<u8> {
    debug_assert!(entries.windows(2).all(|pair| pair[0].id < pair[1].id));
    let count = entries.len();
    let mut out =
        Vec::with_capacity(HEADER_LEN + FANOUT_LEN + count * (32 + LOCATION_LEN) + FOOTER_LEN);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(count as u64).to_le_bytes());
    out.extend_from_slice(&segment_len.to_le_bytes());

    let mut fanout = [0u32; 256];
    for entry in entries {
        fanout[usize::from(entry.id.as_bytes()[0])] += 1;
    }
    let mut total = 0u32;
    for slot in &mut fanout {
        total += *slot;
        *slot = total;
    }
    for value in fanout {
        out.extend_from_slice(&value.to_le_bytes());
    }

    for entry in entries {
        out.extend_from_slice(entry.id.as_bytes());
    }
    for entry in entries {
        out.extend_from_slice(&entry.offset.to_le_bytes());
        out.extend_from_slice(&entry.stored_len.to_le_bytes());
        out.push(entry.kind.code());
        out.push(entry.codec);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&entry.raw_len.to_le_bytes());
        out.extend_from_slice(&entry.crc.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
    }

    let hash = blake3::hash(&out);
    out.extend_from_slice(hash.as_bytes());
    out
}

/// A decoded index.
#[derive(Debug)]
pub(crate) struct Index {
    /// Entries sorted by id.
    pub entries: Vec<Entry>,
    /// The size of the segment when it was sealed.
    pub segment_len: u64,
}

/// Decodes and checks an index.
pub(crate) fn decode(bytes: &[u8]) -> Result<Index, &'static str> {
    if bytes.len() < HEADER_LEN + FANOUT_LEN + FOOTER_LEN {
        return Err("index file cut short");
    }
    let (body, footer) = bytes.split_at(bytes.len() - FOOTER_LEN);
    if blake3::hash(body).as_bytes() != footer {
        return Err("index checksum mismatch");
    }
    if &body[0..8] != MAGIC {
        return Err("not an index file");
    }
    if u32::from_le_bytes(array(&body[8..12])) != VERSION {
        return Err("unknown index version");
    }
    if u32::from_le_bytes(array(&body[12..16])) != 0 {
        return Err("reserved index field is not zero");
    }
    let count = u64::from_le_bytes(array(&body[16..24]));
    let segment_len = u64::from_le_bytes(array(&body[24..32]));
    let count = usize::try_from(count).map_err(|_| "index entry count too large")?;
    let expected = count
        .checked_mul(32 + LOCATION_LEN)
        .and_then(|n| n.checked_add(HEADER_LEN + FANOUT_LEN))
        .ok_or("index entry count too large")?;
    if body.len() != expected {
        return Err("index length does not match its entry count");
    }

    let fanout = &body[HEADER_LEN..HEADER_LEN + FANOUT_LEN];
    let ids = &body[HEADER_LEN + FANOUT_LEN..HEADER_LEN + FANOUT_LEN + count * 32];
    let locations = &body[HEADER_LEN + FANOUT_LEN + count * 32..];

    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let id = &ids[i * 32..i * 32 + 32];
        let location = &locations[i * LOCATION_LEN..(i + 1) * LOCATION_LEN];
        if u32::from_le_bytes(array(&location[28..32])) != 0 {
            return Err("reserved location field is not zero");
        }
        let entry = Entry::from_fields(id, &location[..28]).ok_or("invalid index location")?;
        if entry.end() > segment_len {
            return Err("index entry points past the end of the segment");
        }
        if entries
            .last()
            .is_some_and(|previous: &Entry| previous.id >= entry.id)
        {
            return Err("index ids are not strictly ascending");
        }
        entries.push(entry);
    }

    let mut below = 0usize;
    for (byte, value) in fanout.chunks_exact(4).enumerate() {
        below += entries[below..]
            .iter()
            .take_while(|entry| usize::from(entry.id.as_bytes()[0]) == byte)
            .count();
        if u32::from_le_bytes(array(value)) as usize != below {
            return Err("index fan-out table does not match its ids");
        }
    }
    Ok(Index {
        entries,
        segment_len,
    })
}

#[cfg(test)]
mod tests {
    use braid_format::{Kind, ObjectId};

    use super::*;
    use crate::record;

    fn entries(n: usize) -> Vec<Entry> {
        let mut offset = 0;
        let mut entries: Vec<Entry> = (0..n)
            .map(|i| {
                let payload = format!("object number {i}").repeat(i % 7 + 1);
                let id = ObjectId::hash(Kind::Blob, payload.as_bytes());
                let encoded = record::encode(Kind::Blob, id, payload.as_bytes()).unwrap();
                let entry = Entry::new(offset, &encoded.header, encoded.crc);
                offset += encoded.bytes.len() as u64;
                entry
            })
            .collect();
        entries.sort_by_key(|entry| entry.id);
        entries
    }

    fn segment_len(entries: &[Entry]) -> u64 {
        entries.iter().map(Entry::end).max().unwrap_or(0)
    }

    #[test]
    fn round_trip() {
        for n in [0, 1, 2, 300] {
            let entries = entries(n);
            let len = segment_len(&entries);
            let decoded = decode(&encode(&entries, len)).unwrap();
            assert_eq!(decoded.entries, entries);
            assert_eq!(decoded.segment_len, len);
        }
    }

    #[test]
    fn every_flipped_byte_is_caught() {
        let entries = entries(5);
        let bytes = encode(&entries, segment_len(&entries));
        for i in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[i] ^= 0x10;
            assert!(decode(&damaged).is_err(), "byte {i}");
        }
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "cut at {len}");
        }
    }

    #[test]
    fn structure_is_checked_under_a_valid_checksum() {
        let entries = entries(3);
        let len = segment_len(&entries);
        let reseal = |mut body: Vec<u8>| {
            body.truncate(body.len() - FOOTER_LEN);
            let hash = blake3::hash(&body);
            body.extend_from_slice(hash.as_bytes());
            body
        };

        // Ids out of order.
        let mut swapped = entries.clone();
        swapped.swap(0, 2);
        let mut bytes = encode(&entries, len);
        for (i, entry) in swapped.iter().enumerate() {
            let at = HEADER_LEN + FANOUT_LEN + i * 32;
            bytes[at..at + 32].copy_from_slice(entry.id.as_bytes());
        }
        assert!(decode(&reseal(bytes)).is_err());

        // An entry past the end of the segment.
        assert_eq!(
            decode(&encode(&entries, len - 8)).unwrap_err(),
            "index entry points past the end of the segment"
        );

        // A fan-out table that disagrees with the ids.
        let mut bytes = encode(&entries, len);
        bytes[HEADER_LEN] ^= 1;
        assert!(decode(&reseal(bytes)).is_err());
    }
}
