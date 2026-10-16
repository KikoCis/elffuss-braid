// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Indexes of sealed segments (format spec, section 7).
//!
//! An index lists, in id order, where the records of one or more sealed
//! segments are. It is written once, to a temporary file that is flushed and
//! renamed into place, and never changes afterwards.
//!
//! **Version 2**, which this version writes (storage version 3): the index of
//! one segment, `seg/<name>.idx`, or a merged index of several,
//! `seg/<name>.midx`.
//!
//! ```text
//! [8] "braididx" | u32 version = 2 | u32 0 | u64 count | u32 segments
//! u32 crc32c of the 28 bytes before it
//! segments × ([16] segment name | u64 segment length)
//! 256 × u32 fan-out: entry b counts the ids whose first byte is at most b
//! u32 crc32c of the segment table and the fan-out
//! count × 64-byte entries, ids strictly ascending:
//!     [32] id | u64 offset | u32 stored_len | u8 kind | u8 codec | u16 segment
//!     u64 raw_len | u32 record crc | u32 crc32c of the 60 bytes before it
//! ```
//!
//! An entry is a journal entry whose reserved field holds the number of its
//! segment in the table, 0 in the index of one segment. A segment name is
//! the 32 hex digits of its file name as 16 bytes, and its length the size
//! of the segment when it was sealed. Every part has a checksum of its own,
//! so a reader takes in what a lookup needs and checks only that: the head
//! (everything up to the second checksum) when it first uses the index, and
//! the entries of one fan-out bucket, the ids that share their first byte,
//! when a lookup first needs that bucket. The cost of a lookup follows the
//! size of a bucket, not the size of the index.
//!
//! **Version 1** (storage versions 1 and 2), the index of one segment, read
//! and checked whole:
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
//! An index whose checksums hold but whose version, reserved fields or
//! entries this version does not know was written by a newer version of
//! braid, and is refused rather than taken for damage. The first 32 bytes
//! of every version after 1 carry their own checksum, so that a reader can
//! tell a later version from damage without knowing its layout.

use std::ops::Range;

use crate::journal::{ENTRY_LEN, Entry, Unusable};
use crate::record::array;

/// Why an index cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexError {
    /// It fails its checksums or its structure checks. Lookups work around
    /// it by scanning the segments it covers.
    Damaged(&'static str),
    /// It is whole, but a newer version of braid wrote it.
    Newer(&'static str),
}

const MAGIC: &[u8; 8] = b"braididx";
const VERSION_1: u32 = 1;
const VERSION_2: u32 = 2;
/// Bytes of the fixed header every version starts with.
pub(crate) const HEADER_LEN: usize = 32;
/// Bytes of one segment in the table of a version 2 index.
const COVERED_LEN: usize = 24;
const FANOUT_LEN: usize = 256 * 4;
const CRC_LEN: usize = 4;
/// Bytes of a version 1 location.
const V1_LOCATION_LEN: usize = 32;
/// Bytes of the hash that ends a version 1 index.
const V1_FOOTER_LEN: usize = 32;

/// The most segments one index covers: entries number theirs with a u16.
pub(crate) const MAX_COVERED: usize = 1 << 16;

/// An entry, with the number of its segment in the index that holds it.
pub(crate) type Numbered = (Entry, u16);

/// A sealed segment an index covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Covered {
    /// Its name: 32 lowercase hex digits.
    pub name: String,
    /// Its length when it was sealed. No entry points past it.
    pub len: u64,
}

/// What the fixed header of an index says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Header {
    /// A version 1 index: it is read and checked whole.
    V1,
    /// A version 2 index of `count` entries for `segments` segments.
    V2 { count: u64, segments: usize },
}

impl Header {
    /// Bytes of a version 2 index's head, from the start of the file to its
    /// first entry.
    pub fn head_len(segments: usize) -> usize {
        HEADER_LEN + COVERED_LEN * segments + FANOUT_LEN + CRC_LEN
    }
}

/// Reads the fixed header, the first [`HEADER_LEN`] bytes of an index.
pub(crate) fn header(bytes: &[u8]) -> Result<Header, IndexError> {
    use IndexError::{Damaged, Newer};
    if bytes.len() < HEADER_LEN {
        return Err(Damaged("index file cut short"));
    }
    if &bytes[0..8] != MAGIC {
        return Err(Damaged("not an index file"));
    }
    let version = u32::from_le_bytes(array(&bytes[8..12]));
    if version == VERSION_1 {
        // Its hash, at the end of the file, says whether it is whole.
        return Ok(Header::V1);
    }
    if crc32c::crc32c(&bytes[..28]) != u32::from_le_bytes(array(&bytes[28..32])) {
        return Err(Damaged("index header checksum mismatch"));
    }
    // Whole from here on: what this version does not know is news.
    match version {
        VERSION_2 => {}
        0 => return Err(Damaged("index version 0")),
        _ => return Err(Newer("an index version this version does not know")),
    }
    if u32::from_le_bytes(array(&bytes[12..16])) != 0 {
        return Err(Newer("a reserved index field is set"));
    }
    let count = u64::from_le_bytes(array(&bytes[16..24]));
    let segments = u32::from_le_bytes(array(&bytes[24..28])) as usize;
    if segments == 0 || segments > MAX_COVERED {
        return Err(Damaged("index covers no segment, or too many"));
    }
    Ok(Header::V2 { count, segments })
}

/// The head of a version 2 index: what it covers, and where its entries are.
#[derive(Clone, Debug)]
pub(crate) struct Head {
    /// The segments it covers, by their number in it.
    pub covered: Vec<Covered>,
    /// How many entries it holds.
    pub count: u64,
    /// Entry `b` is the number of entries whose id's first byte is at most `b`.
    fanout: Vec<u32>,
}

impl Head {
    /// Bytes from the start of the file to the first entry.
    pub fn len(&self) -> u64 {
        Header::head_len(self.covered.len()) as u64
    }

    /// Bytes of the whole index.
    pub fn file_len(&self) -> u64 {
        self.len() + self.count * ENTRY_LEN
    }

    /// The numbers of the entries whose ids start with `byte`.
    pub fn bucket(&self, byte: u8) -> Range<u64> {
        let end = u64::from(self.fanout[usize::from(byte)]);
        let start = match byte {
            0 => 0,
            _ => u64::from(self.fanout[usize::from(byte) - 1]),
        };
        start..end
    }

    /// Where entry `n` starts in the file.
    pub fn entry_offset(&self, n: u64) -> u64 {
        self.len() + n * ENTRY_LEN
    }
}

/// Reads the head of a version 2 index: its first
/// [`Header::head_len`] bytes.
pub(crate) fn head(bytes: &[u8]) -> Result<Head, IndexError> {
    use IndexError::Damaged;
    let Header::V2 { count, segments } = header(bytes)? else {
        return Err(Damaged("a version 1 index has no head of its own"));
    };
    let len = Header::head_len(segments);
    if bytes.len() < len {
        return Err(Damaged("index file cut short"));
    }
    let checked = &bytes[HEADER_LEN..len - CRC_LEN];
    if crc32c::crc32c(checked) != u32::from_le_bytes(array(&bytes[len - CRC_LEN..len])) {
        return Err(Damaged("index head checksum mismatch"));
    }
    let (table, fanout) = checked.split_at(COVERED_LEN * segments);
    let mut covered = Vec::with_capacity(segments);
    for slot in table.chunks_exact(COVERED_LEN) {
        let name = hex(&slot[..16]);
        if covered.iter().any(|seen: &Covered| seen.name == name) {
            return Err(Damaged("index names a segment twice"));
        }
        covered.push(Covered {
            name,
            len: u64::from_le_bytes(array(&slot[16..24])),
        });
    }
    let fanout: Vec<u32> = fanout
        .chunks_exact(4)
        .map(|value| u32::from_le_bytes(array(value)))
        .collect();
    if fanout.windows(2).any(|pair| pair[0] > pair[1]) || u64::from(fanout[255]) != count {
        return Err(Damaged(
            "index fan-out table does not match its entry count",
        ));
    }
    Ok(Head {
        covered,
        count,
        fanout,
    })
}

/// Checks and decodes the entries of the bucket of ids that start with
/// `byte`, read from the file as [`Head::bucket`] says.
pub(crate) fn bucket(head: &Head, byte: u8, bytes: &[u8]) -> Result<Vec<(Entry, u16)>, IndexError> {
    let expected = head.bucket(byte);
    if bytes.len() as u64 != (expected.end - expected.start) * ENTRY_LEN {
        return Err(IndexError::Damaged("index bucket cut short"));
    }
    let mut entries: Vec<(Entry, u16)> = Vec::with_capacity(bytes.len() / ENTRY_LEN as usize);
    for slot in bytes.chunks_exact(ENTRY_LEN as usize) {
        let (entry, segment) = Entry::decode_in(slot).map_err(|unusable| match unusable {
            Unusable::Invalid => IndexError::Damaged("index entry fails its checksum"),
            Unusable::Newer => {
                IndexError::Newer("an object kind or codec this version does not know")
            }
        })?;
        if entry.id.as_bytes()[0] != byte
            || entries
                .last()
                .is_some_and(|(previous, _)| previous.id >= entry.id)
        {
            return Err(IndexError::Damaged("index ids are not strictly ascending"));
        }
        let Some(covered) = head.covered.get(usize::from(segment)) else {
            return Err(IndexError::Damaged(
                "index entry names a segment it does not cover",
            ));
        };
        if entry.end() > covered.len {
            return Err(IndexError::Damaged(
                "index entry points past the end of the segment",
            ));
        }
        entries.push((entry, segment));
    }
    Ok(entries)
}

/// Encodes a version 2 index of `covered`. `entries` must be sorted by id,
/// without duplicates, each with the number of its segment in `covered`.
pub(crate) fn encode(covered: &[Covered], entries: &[(Entry, u16)]) -> Vec<u8> {
    debug_assert!(!covered.is_empty() && covered.len() <= MAX_COVERED);
    debug_assert!(entries.windows(2).all(|pair| pair[0].0.id < pair[1].0.id));
    let head_len = Header::head_len(covered.len());
    let mut out = Vec::with_capacity(head_len + entries.len() * ENTRY_LEN as usize);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION_2.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    out.extend_from_slice(&(covered.len() as u32).to_le_bytes());
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    for segment in covered {
        out.extend_from_slice(&unhex(&segment.name));
        out.extend_from_slice(&segment.len.to_le_bytes());
    }
    let mut fanout = [0u32; 256];
    for (entry, _) in entries {
        fanout[usize::from(entry.id.as_bytes()[0])] += 1;
    }
    let mut total = 0u32;
    for slot in &mut fanout {
        total += *slot;
        *slot = total;
        out.extend_from_slice(&slot.to_le_bytes());
    }
    let crc = crc32c::crc32c(&out[HEADER_LEN..]);
    out.extend_from_slice(&crc.to_le_bytes());
    for (entry, segment) in entries {
        out.extend_from_slice(&entry.encode_in(*segment));
    }
    out
}

/// A whole index, decoded and checked.
#[derive(Debug)]
pub(crate) struct Decoded {
    /// The segments it covers. A version 1 index covers the segment its file
    /// is named after.
    pub covered: Vec<Covered>,
    /// Its entries, sorted by id, each with the number of its segment.
    pub entries: Vec<(Entry, u16)>,
}

/// Decodes and checks a whole index of either version. `own` is the name of
/// the segment whose own index this is, or `None` for a merged index.
pub(crate) fn decode(bytes: &[u8], own: Option<&str>) -> Result<Decoded, IndexError> {
    use IndexError::Damaged;
    let decoded = match header(bytes)? {
        Header::V1 => {
            let Some(name) = own else {
                return Err(Damaged("a merged index of version 1"));
            };
            let (entries, len) = decode_v1(bytes)?;
            Decoded {
                covered: vec![Covered {
                    name: name.to_owned(),
                    len,
                }],
                entries: entries.into_iter().map(|entry| (entry, 0)).collect(),
            }
        }
        Header::V2 { .. } => {
            let head = head(bytes)?;
            if bytes.len() as u64 != head.file_len() {
                return Err(Damaged("index length does not match its entry count"));
            }
            let mut entries = Vec::with_capacity(head.count as usize);
            for byte in 0..=255u8 {
                let range = head.bucket(byte);
                let start = head.entry_offset(range.start) as usize;
                let end = head.entry_offset(range.end) as usize;
                entries.extend(bucket(&head, byte, &bytes[start..end])?);
            }
            Decoded {
                covered: head.covered,
                entries,
            }
        }
    };
    if let Some(name) = own
        && (decoded.covered.len() != 1 || decoded.covered[0].name != name)
    {
        return Err(Damaged("the index of another segment"));
    }
    Ok(decoded)
}

/// Decodes and checks a version 1 index: its entries and the segment's
/// length when it was sealed.
fn decode_v1(bytes: &[u8]) -> Result<(Vec<Entry>, u64), IndexError> {
    use IndexError::{Damaged, Newer};
    if bytes.len() < HEADER_LEN + FANOUT_LEN + V1_FOOTER_LEN {
        return Err(Damaged("index file cut short"));
    }
    let (body, footer) = bytes.split_at(bytes.len() - V1_FOOTER_LEN);
    if blake3::hash(body).as_bytes() != footer {
        return Err(Damaged("index checksum mismatch"));
    }
    // Whole from here on: what this version does not know is news, not
    // damage.
    if u32::from_le_bytes(array(&body[12..16])) != 0 {
        return Err(Newer("a reserved index field is set"));
    }
    let count = u64::from_le_bytes(array(&body[16..24]));
    let segment_len = u64::from_le_bytes(array(&body[24..32]));
    let count = usize::try_from(count).map_err(|_| Damaged("index entry count too large"))?;
    let expected = count
        .checked_mul(32 + V1_LOCATION_LEN)
        .and_then(|n| n.checked_add(HEADER_LEN + FANOUT_LEN))
        .ok_or(Damaged("index entry count too large"))?;
    if body.len() != expected {
        return Err(Damaged("index length does not match its entry count"));
    }

    let fanout = &body[HEADER_LEN..HEADER_LEN + FANOUT_LEN];
    let ids = &body[HEADER_LEN + FANOUT_LEN..HEADER_LEN + FANOUT_LEN + count * 32];
    let locations = &body[HEADER_LEN + FANOUT_LEN + count * 32..];

    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let id = &ids[i * 32..i * 32 + 32];
        let location = &locations[i * V1_LOCATION_LEN..(i + 1) * V1_LOCATION_LEN];
        if u32::from_le_bytes(array(&location[28..32])) != 0 {
            return Err(Newer("a reserved location field is set"));
        }
        let entry = Entry::from_fields(id, &location[..28]).map_err(|unusable| match unusable {
            Unusable::Invalid => Damaged("invalid index location"),
            Unusable::Newer => Newer("an object kind or codec this version does not know"),
        })?;
        if entry.end() > segment_len {
            return Err(Damaged("index entry points past the end of the segment"));
        }
        if entries
            .last()
            .is_some_and(|previous: &Entry| previous.id >= entry.id)
        {
            return Err(Damaged("index ids are not strictly ascending"));
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
            return Err(Damaged("index fan-out table does not match its ids"));
        }
    }
    Ok((entries, segment_len))
}

/// The 32 hex digits of a 16-byte segment name.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// The 16 bytes of a segment name, 32 lowercase hex digits.
fn unhex(name: &str) -> [u8; 16] {
    debug_assert!(crate::fsutil::is_random_name(name));
    let digit = |b: u8| match b {
        b'0'..=b'9' => b - b'0',
        _ => b - b'a' + 10,
    };
    let mut out = [0u8; 16];
    for (slot, pair) in out.iter_mut().zip(name.as_bytes().chunks_exact(2)) {
        *slot = digit(pair[0]) << 4 | digit(pair[1]);
    }
    out
}

/// Encodes a version 1 index, as storage versions 1 and 2 wrote them, for
/// tests of stores that older versions left.
#[cfg(test)]
pub(crate) fn encode_v1(entries: &[Entry], segment_len: u64) -> Vec<u8> {
    let count = entries.len();
    let mut out = Vec::with_capacity(
        HEADER_LEN + FANOUT_LEN + count * (32 + V1_LOCATION_LEN) + V1_FOOTER_LEN,
    );
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION_1.to_le_bytes());
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
        out.extend_from_slice(&slot.to_le_bytes());
    }
    for entry in entries {
        out.extend_from_slice(entry.id.as_bytes());
    }
    for entry in entries {
        out.extend_from_slice(&entry.encode()[32..60]);
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    let hash = blake3::hash(&out);
    out.extend_from_slice(hash.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use braid_format::{Kind, ObjectId};

    use super::*;
    use crate::record;

    fn entries(n: usize) -> Vec<Entry> {
        entries_of(0..n)
    }

    fn entries_of(objects: std::ops::Range<usize>) -> Vec<Entry> {
        let mut offset = 0;
        let mut entries: Vec<Entry> = objects
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

    const NAME: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";

    fn own(entries: &[Entry]) -> Vec<u8> {
        let covered = [Covered {
            name: NAME.to_owned(),
            len: segment_len(entries),
        }];
        let entries: Vec<(Entry, u16)> = entries.iter().map(|entry| (*entry, 0)).collect();
        encode(&covered, &entries)
    }

    #[test]
    fn round_trip() {
        for n in [0, 1, 2, 300] {
            let entries = entries(n);
            let len = segment_len(&entries);
            let decoded = decode(&own(&entries), Some(NAME)).unwrap();
            assert_eq!(
                decoded.entries,
                entries.iter().map(|e| (*e, 0)).collect::<Vec<_>>()
            );
            assert_eq!(
                decoded.covered,
                [Covered {
                    name: NAME.to_owned(),
                    len
                }]
            );
            // Version 1, as older versions wrote it.
            let decoded = decode(&encode_v1(&entries, len), Some(NAME)).unwrap();
            assert_eq!(decoded.entries.len(), n);
            assert_eq!(decoded.covered[0].len, len);
        }
    }

    #[test]
    fn a_merged_index_names_each_entry_segment() {
        let first = entries_of(0..40);
        let second = entries_of(40..80);
        let covered = vec![
            Covered {
                name: NAME.to_owned(),
                len: segment_len(&first),
            },
            Covered {
                name: OTHER.to_owned(),
                len: segment_len(&second),
            },
        ];
        let mut merged: Vec<(Entry, u16)> = first
            .iter()
            .map(|e| (*e, 0))
            .chain(second.iter().map(|e| (*e, 1)))
            .collect();
        merged.sort_by_key(|(entry, _)| entry.id);
        let bytes = encode(&covered, &merged);
        let decoded = decode(&bytes, None).unwrap();
        assert_eq!(decoded.covered, covered);
        assert_eq!(decoded.entries, merged);
        // Read a bucket at a time, as lookups do.
        let head = head(&bytes[..Header::head_len(2)]).unwrap();
        assert_eq!(head.file_len(), bytes.len() as u64);
        let mut seen = Vec::new();
        for byte in 0..=255u8 {
            let range = head.bucket(byte);
            let at = |n| head.entry_offset(n) as usize;
            seen.extend(bucket(&head, byte, &bytes[at(range.start)..at(range.end)]).unwrap());
        }
        assert_eq!(seen, merged);
        // An index of one segment is not another segment's.
        assert!(decode(&bytes, Some(NAME)).is_err());
        assert!(decode(&own(&first), Some(OTHER)).is_err());
    }

    #[test]
    fn every_flipped_byte_is_caught() {
        let entries = entries(5);
        let bytes = own(&entries);
        for i in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[i] ^= 0x10;
            assert!(decode(&damaged, Some(NAME)).is_err(), "byte {i}");
        }
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len], Some(NAME)).is_err(), "cut at {len}");
        }
        let v1 = encode_v1(&entries, segment_len(&entries));
        for i in 0..v1.len() {
            let mut damaged = v1.clone();
            damaged[i] ^= 0x10;
            assert!(decode(&damaged, Some(NAME)).is_err(), "version 1, byte {i}");
        }
    }

    #[test]
    fn structure_is_checked_under_valid_checksums() {
        let entries = entries(3);
        let len = segment_len(&entries);
        let covered = [Covered {
            name: NAME.to_owned(),
            len,
        }];
        let with = |list: &[(Entry, u16)], covered: &[Covered]| {
            let mut bytes = encode(covered, &[]);
            // Entries written as given, with the fan-out fixed to match.
            let mut fanout = [0u32; 256];
            for (entry, _) in list {
                fanout[usize::from(entry.id.as_bytes()[0])] += 1;
            }
            let mut total = 0;
            let start = HEADER_LEN + COVERED_LEN * covered.len();
            for (byte, slot) in fanout.iter().enumerate() {
                total += slot;
                bytes[start + byte * 4..start + byte * 4 + 4].copy_from_slice(&total.to_le_bytes());
            }
            bytes[16..24].copy_from_slice(&(list.len() as u64).to_le_bytes());
            let crc = crc32c::crc32c(&bytes[..28]);
            bytes[28..32].copy_from_slice(&crc.to_le_bytes());
            let head_end = Header::head_len(covered.len());
            let crc = crc32c::crc32c(&bytes[HEADER_LEN..head_end - CRC_LEN]);
            bytes[head_end - CRC_LEN..head_end].copy_from_slice(&crc.to_le_bytes());
            for (entry, segment) in list {
                bytes.extend_from_slice(&entry.encode_in(*segment));
            }
            bytes
        };
        let good: Vec<(Entry, u16)> = entries.iter().map(|e| (*e, 0)).collect();
        assert!(decode(&with(&good, &covered), Some(NAME)).is_ok());

        // Ids out of order.
        let mut swapped = good.clone();
        swapped.swap(0, 2);
        assert!(matches!(
            decode(&with(&swapped, &covered), Some(NAME)),
            Err(IndexError::Damaged(_))
        ));
        // An entry past the end of its segment.
        let short = [Covered {
            name: NAME.to_owned(),
            len: len - 8,
        }];
        assert_eq!(
            decode(&with(&good, &short), Some(NAME)).unwrap_err(),
            IndexError::Damaged("index entry points past the end of the segment")
        );
        // An entry naming a segment the index does not cover.
        let mut stray = good.clone();
        stray[1].1 = 1;
        assert_eq!(
            decode(&with(&stray, &covered), Some(NAME)).unwrap_err(),
            IndexError::Damaged("index entry names a segment it does not cover")
        );
    }

    #[test]
    fn what_a_newer_version_wrote_is_not_damage() {
        let entries = entries(3);
        // A later version or a reserved field in a header that checks out.
        for (at, value) in [(8, 3u8), (12, 1)] {
            let mut bytes = own(&entries);
            bytes[at] = value;
            let crc = crc32c::crc32c(&bytes[..28]);
            bytes[28..32].copy_from_slice(&crc.to_le_bytes());
            assert!(
                matches!(decode(&bytes, Some(NAME)), Err(IndexError::Newer(_))),
                "byte {at}"
            );
        }
        // An entry with a codec this version does not know.
        let mut bytes = own(&entries);
        let at = Header::head_len(1);
        bytes[at + 45] = 9;
        let crc = crc32c::crc32c(&bytes[at..at + 60]);
        bytes[at + 60..at + 64].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            decode(&bytes, Some(NAME)),
            Err(IndexError::Newer(_))
        ));

        // Version 1: a later version, a reserved field, an unknown codec, an
        // unknown kind, a reserved location field, under a valid hash.
        let len = segment_len(&entries);
        let reseal = |mut body: Vec<u8>| {
            body.truncate(body.len() - V1_FOOTER_LEN);
            let hash = blake3::hash(&body);
            body.extend_from_slice(hash.as_bytes());
            body
        };
        let locations = HEADER_LEN + FANOUT_LEN + entries.len() * 32;
        for at in [12, locations + 13, locations + 12, locations + 28] {
            let mut bytes = encode_v1(&entries, len);
            bytes[at] = 7;
            assert!(
                matches!(
                    decode(&reseal(bytes), Some(NAME)),
                    Err(IndexError::Newer(_))
                ),
                "byte {at}"
            );
        }
    }

    #[test]
    fn names_round_trip() {
        assert_eq!(hex(&unhex(NAME)), NAME);
        assert_eq!(hex(&unhex(OTHER)), OTHER);
    }
}
