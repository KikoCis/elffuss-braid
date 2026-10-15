// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Object records, the contents of segment files (format spec, section 7).
//!
//! ```text
//! u32 sync = 0xB7A1D5E9 | u8 kind | u8 codec | u16 flags = 0
//! u32 stored_len | u64 raw_len | [32] id
//! codecs 2 and 3 only: [32] dictionary or base id
//! payload: stored_len bytes
//! u32 crc32c of everything from the sync word to the end of the payload
//! zero padding to a multiple of 8 bytes
//! ```
//!
//! Integers are little-endian. Records start at multiples of 8 bytes, so a
//! reader that meets the remains of a write cut short can find the next
//! record by looking for the sync word 8 bytes at a time.
//!
//! The codec says how the payload is stored:
//!
//! - 0, raw: as it is. Payloads of up to 64 bytes are always raw.
//! - 1, zstd: one zstd frame at level 3, used when it is smaller than raw.
//! - 2, reserved for zstd with a dictionary: its header is parsed, so a scan
//!   can step over it, but nothing writes or decodes it.
//! - 3, delta: one zstd frame compressed with the raw payload of the base
//!   object as a reference prefix (see [`crate::delta`]). The base is stored
//!   whole, with codec 0 or 1.
//! - 4, chunked, for blobs only: the payload is the list of the chunks the
//!   blob is cut into (see [`crate::chunks`]).
//!
//! Decoding codecs 0 and 1 needs the record alone, and is done here. Codecs 3
//! and 4 need other objects, so the segments resolve them.

use std::borrow::Cow;
use std::io::Read;

use braid_format::{Kind, ObjectId};

/// The first four bytes of every record.
pub(crate) const SYNC: u32 = 0xB7A1_D5E9;

/// Bytes from the sync word to the payload, for codecs without a reference.
pub(crate) const HEADER_LEN: usize = 52;

/// Bytes of the dictionary or base id that codecs 2 and 3 add to the header.
pub(crate) const REFERENCE_LEN: usize = 32;

/// Records start at multiples of this.
pub(crate) const ALIGN: u64 = 8;

/// Payloads up to this size are always stored raw.
pub(crate) const RAW_MAX: usize = 64;

/// The zstd level for compressed payloads and deltas.
pub(crate) const ZSTD_LEVEL: i32 = 3;

/// The payload is stored as it is.
pub(crate) const CODEC_RAW: u8 = 0;
/// The payload is one zstd frame.
pub(crate) const CODEC_ZSTD: u8 = 1;
/// Reserved: one zstd frame compressed with a dictionary.
pub(crate) const CODEC_DICTIONARY: u8 = 2;
/// The payload is one zstd frame compressed against a base object.
pub(crate) const CODEC_DELTA: u8 = 3;
/// The payload is the chunk list of a blob stored in chunks.
pub(crate) const CODEC_CHUNKED: u8 = 4;
/// The highest codec number the format defines.
pub(crate) const CODEC_MAX: u8 = CODEC_CHUNKED;

/// Bytes the CRC adds after the payload.
const CRC_LEN: usize = 4;

/// Whether a record of this codec holds its object's payload by itself, so
/// that it can be a delta's base.
pub(crate) fn is_whole(codec: u8) -> bool {
    codec == CODEC_RAW || codec == CODEC_ZSTD
}

/// How readers rank copies of one object, best first: whole records, then
/// deltas, then chunked blobs, then what this version cannot decode. When a
/// handle knows several copies of an object it uses the best ranked one, so
/// that a delta's base resolves to a whole copy whenever one is known.
pub(crate) fn rank(codec: u8) -> u8 {
    match codec {
        CODEC_RAW | CODEC_ZSTD => 0,
        CODEC_DELTA => 1,
        CODEC_CHUNKED => 2,
        _ => 3,
    }
}

/// The fixed fields of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    /// What the object is.
    pub kind: Kind,
    /// How the payload is stored.
    pub codec: u8,
    /// Bytes of payload in the record.
    pub stored_len: u32,
    /// Bytes of the object once decoded.
    pub raw_len: u64,
    /// The object's id.
    pub id: ObjectId,
}

impl Header {
    /// Bytes from the sync word to the start of the payload.
    pub fn header_len(&self) -> u64 {
        header_len(self.codec) as u64
    }

    /// Bytes the CRC covers: the header and the payload.
    pub fn checked_len(&self) -> u64 {
        self.header_len() + u64::from(self.stored_len)
    }

    /// Bytes to read to check the record: the header, the payload and the CRC.
    pub fn unpadded_len(&self) -> u64 {
        self.checked_len() + CRC_LEN as u64
    }

    /// Bytes the record takes in its segment, padding included.
    pub fn record_len(&self) -> u64 {
        align(self.unpadded_len())
    }
}

/// Bytes from the sync word to the payload for a codec.
pub(crate) fn header_len(codec: u8) -> usize {
    if has_reference(codec) {
        HEADER_LEN + REFERENCE_LEN
    } else {
        HEADER_LEN
    }
}

/// Whether records of this codec name another object in their header.
fn has_reference(codec: u8) -> bool {
    codec == CODEC_DICTIONARY || codec == CODEC_DELTA
}

/// Rounds up to the next record boundary.
pub(crate) fn align(offset: u64) -> u64 {
    offset.next_multiple_of(ALIGN)
}

/// A record ready to append.
#[derive(Debug)]
pub(crate) struct Encoded {
    /// Its fixed fields.
    pub header: Header,
    /// Its CRC.
    pub crc: u32,
    /// All of its bytes, padding included.
    pub bytes: Vec<u8>,
}

/// The stored form of a payload on its own: raw when it is small or does
/// not compress, else one zstd frame.
pub(crate) fn standalone(payload: &[u8]) -> (u8, Cow<'_, [u8]>) {
    if payload.len() > RAW_MAX
        && let Ok(compressed) = zstd::bulk::compress(payload, ZSTD_LEVEL)
        && compressed.len() < payload.len()
    {
        return (CODEC_ZSTD, Cow::Owned(compressed));
    }
    (CODEC_RAW, Cow::Borrowed(payload))
}

/// Encodes the object `id`, which must be the hash of `payload` as `kind`,
/// on its own: raw or compressed, whichever is smaller. Fails with the
/// payload size if the stored form does not fit in a record.
pub(crate) fn encode(kind: Kind, id: ObjectId, payload: &[u8]) -> Result<Encoded, u64> {
    let (codec, stored) = standalone(payload);
    assemble(kind, id, codec, payload.len() as u64, None, &stored)
}

/// Encodes the object `id` raw, without trying to compress it.
pub(crate) fn encode_raw(kind: Kind, id: ObjectId, payload: &[u8]) -> Result<Encoded, u64> {
    assemble(kind, id, CODEC_RAW, payload.len() as u64, None, payload)
}

/// Builds a record from its fields: `stored` is the payload as stored, and
/// `reference` the base or dictionary id of codecs 2 and 3. Fails with
/// `raw_len` if the stored form does not fit in a record.
pub(crate) fn assemble(
    kind: Kind,
    id: ObjectId,
    codec: u8,
    raw_len: u64,
    reference: Option<&ObjectId>,
    stored: &[u8],
) -> Result<Encoded, u64> {
    debug_assert_eq!(reference.is_some(), has_reference(codec));
    let stored_len = u32::try_from(stored.len()).map_err(|_| raw_len)?;
    let header = Header {
        kind,
        codec,
        stored_len,
        raw_len,
        id,
    };
    let record_len = usize::try_from(header.record_len()).map_err(|_| raw_len)?;
    let mut bytes = Vec::with_capacity(record_len);
    bytes.extend_from_slice(&SYNC.to_le_bytes());
    bytes.push(kind.code());
    bytes.push(codec);
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&stored_len.to_le_bytes());
    bytes.extend_from_slice(&raw_len.to_le_bytes());
    bytes.extend_from_slice(id.as_bytes());
    if let Some(reference) = reference {
        bytes.extend_from_slice(reference.as_bytes());
    }
    bytes.extend_from_slice(stored);
    let crc = crc32c::crc32c(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes.resize(record_len, 0);
    Ok(Encoded { header, crc, bytes })
}

/// Parses the fixed fields at the start of `bytes`, rejecting anything a
/// writer would never produce.
pub(crate) fn parse_header(bytes: &[u8]) -> Result<Header, &'static str> {
    if bytes.len() < HEADER_LEN {
        return Err("record header cut short");
    }
    if u32::from_le_bytes(array(&bytes[0..4])) != SYNC {
        return Err("no sync word");
    }
    let kind = Kind::from_code(bytes[4]).ok_or("unknown object kind")?;
    let codec = bytes[5];
    if codec > CODEC_MAX {
        return Err("unknown codec");
    }
    if u16::from_le_bytes(array(&bytes[6..8])) != 0 {
        return Err("unknown flags");
    }
    let stored_len = u32::from_le_bytes(array(&bytes[8..12]));
    let raw_len = u64::from_le_bytes(array(&bytes[12..20]));
    if codec == CODEC_RAW && raw_len != u64::from(stored_len) {
        return Err("raw payload with two different lengths");
    }
    if codec == CODEC_CHUNKED && kind != Kind::Blob {
        return Err("only blobs are stored in chunks");
    }
    let id = ObjectId::from_bytes(array(&bytes[20..52]));
    Ok(Header {
        kind,
        codec,
        stored_len,
        raw_len,
        id,
    })
}

/// A record whose checksum is valid, read from the bytes that hold it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Checked<'a> {
    /// Its fixed fields.
    pub header: Header,
    /// Its CRC.
    pub crc: u32,
    /// The base (codec 3) or dictionary (codec 2) it names.
    pub reference: Option<ObjectId>,
    /// The payload as stored.
    pub stored: &'a [u8],
}

/// Checks the record at the start of `bytes`, which must hold at least its
/// unpadded length: the header and the CRC.
pub(crate) fn check(bytes: &[u8]) -> Result<Checked<'_>, &'static str> {
    let header = parse_header(bytes)?;
    let checked = usize::try_from(header.checked_len()).map_err(|_| "record too large")?;
    if bytes.len() < checked + CRC_LEN {
        return Err("record cut short");
    }
    let crc = u32::from_le_bytes(array(&bytes[checked..checked + CRC_LEN]));
    if crc32c::crc32c(&bytes[..checked]) != crc {
        return Err("checksum mismatch");
    }
    let reference = has_reference(header.codec)
        .then(|| ObjectId::from_bytes(array(&bytes[HEADER_LEN..HEADER_LEN + REFERENCE_LEN])));
    let start = header_len(header.codec);
    Ok(Checked {
        header,
        crc,
        reference,
        stored: &bytes[start..checked],
    })
}

/// Restores the payload of a checked record stored whole (codec 0 or 1)
/// and verifies that it hashes to the record's id.
pub(crate) fn decode(header: &Header, stored: &[u8]) -> Result<Vec<u8>, &'static str> {
    let payload = match header.codec {
        CODEC_RAW => stored.to_vec(),
        CODEC_ZSTD => decompress(stored, header.raw_len)?,
        CODEC_DELTA | CODEC_CHUNKED => return Err("record needs other objects to decode"),
        _ => return Err("codec not supported by this version"),
    };
    verify(header, &payload)?;
    Ok(payload)
}

/// Checks that a decoded payload hashes to its record's id.
pub(crate) fn verify(header: &Header, payload: &[u8]) -> Result<(), &'static str> {
    if ObjectId::hash(header.kind, payload) != header.id {
        return Err("content does not hash to its id");
    }
    Ok(())
}

fn decompress(stored: &[u8], raw_len: u64) -> Result<Vec<u8>, &'static str> {
    let decoder = zstd::stream::read::Decoder::with_buffer(stored).map_err(|_| "bad zstd data")?;
    read_capped(decoder, raw_len)
}

/// Reads a decoder to its end, expecting exactly `raw_len` bytes. It streams
/// with a cap instead of trusting `raw_len` for one allocation: a damaged
/// length then costs at most the data actually there.
pub(crate) fn read_capped(decoder: impl Read, raw_len: u64) -> Result<Vec<u8>, &'static str> {
    let mut payload = Vec::with_capacity(usize::try_from(raw_len.min(1 << 24)).unwrap_or(0));
    decoder
        .take(raw_len.saturating_add(1))
        .read_to_end(&mut payload)
        .map_err(|_| "bad zstd data")?;
    if payload.len() as u64 != raw_len {
        return Err("decoded length does not match the record");
    }
    Ok(payload)
}

/// Copies a slice of known length into an array.
pub(crate) fn array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(kind: Kind, payload: &[u8]) -> Encoded {
        let id = ObjectId::hash(kind, payload);
        let encoded = encode(kind, id, payload).unwrap();
        assert_eq!(encoded.bytes.len() as u64, encoded.header.record_len());
        assert_eq!(encoded.bytes.len() % ALIGN as usize, 0);
        let checked = check(&encoded.bytes).unwrap();
        assert_eq!(checked.header, encoded.header);
        assert_eq!(checked.crc, encoded.crc);
        assert_eq!(checked.reference, None);
        assert_eq!(decode(&checked.header, checked.stored).unwrap(), payload);
        encoded
    }

    #[test]
    fn small_payloads_are_raw() {
        for len in [0, 1, 63, 64] {
            let payload = vec![b'a'; len];
            let encoded = round_trip(Kind::Blob, &payload);
            assert_eq!(encoded.header.codec, CODEC_RAW, "{len}");
            assert_eq!(encoded.header.stored_len as usize, len);
        }
    }

    #[test]
    fn compressible_payloads_use_zstd() {
        let payload = b"the same line again and again\n".repeat(100);
        let encoded = round_trip(Kind::Tree, &payload);
        assert_eq!(encoded.header.codec, CODEC_ZSTD);
        assert!((encoded.header.stored_len as usize) < payload.len());
        assert_eq!(encoded.header.raw_len as usize, payload.len());
    }

    #[test]
    fn incompressible_payloads_stay_raw() {
        let payload: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let id = ObjectId::hash(Kind::Blob, &payload);
        if zstd::bulk::compress(&payload, ZSTD_LEVEL).unwrap().len() >= payload.len() {
            assert_eq!(
                encode(Kind::Blob, id, &payload).unwrap().header.codec,
                CODEC_RAW
            );
        }
        round_trip(Kind::Blob, &payload);
        // Asked for raw, it stays raw even when it would compress.
        let text = b"compress me please ".repeat(20);
        let id = ObjectId::hash(Kind::Blob, &text);
        let raw = encode_raw(Kind::Blob, id, &text).unwrap();
        assert_eq!(raw.header.codec, CODEC_RAW);
        let checked = check(&raw.bytes).unwrap();
        assert_eq!(decode(&checked.header, checked.stored).unwrap(), text);
    }

    #[test]
    fn layout_matches_the_spec() {
        let encoded = round_trip(Kind::Blob, b"hello\n");
        let bytes = &encoded.bytes;
        assert_eq!(&bytes[0..4], &[0xE9, 0xD5, 0xA1, 0xB7]);
        assert_eq!(bytes[4], 1);
        assert_eq!(bytes[5], CODEC_RAW);
        assert_eq!(&bytes[6..8], &[0, 0]);
        assert_eq!(&bytes[8..12], &6u32.to_le_bytes());
        assert_eq!(&bytes[12..20], &6u64.to_le_bytes());
        assert_eq!(
            &bytes[20..52],
            ObjectId::hash(Kind::Blob, b"hello\n").as_bytes()
        );
        assert_eq!(&bytes[52..58], b"hello\n");
        let crc = crc32c::crc32c(&bytes[..58]);
        assert_eq!(&bytes[58..62], &crc.to_le_bytes());
        assert_eq!(bytes.len(), 64);
        assert_eq!(&bytes[62..], &[0, 0]);
    }

    #[test]
    fn a_reference_follows_the_id() {
        let base = ObjectId::hash(Kind::Blob, b"base");
        let id = ObjectId::hash(Kind::Blob, b"not decoded here");
        let encoded = assemble(Kind::Blob, id, CODEC_DELTA, 16, Some(&base), b"frame").unwrap();
        let bytes = &encoded.bytes;
        assert_eq!(bytes[5], CODEC_DELTA);
        assert_eq!(&bytes[52..84], base.as_bytes());
        assert_eq!(&bytes[84..89], b"frame");
        assert_eq!(encoded.header.header_len(), 84);
        let checked = check(bytes).unwrap();
        assert_eq!(checked.reference, Some(base));
        assert_eq!(checked.stored, b"frame");
        assert_eq!(
            decode(&checked.header, checked.stored),
            Err("record needs other objects to decode")
        );

        // A chunk list has no reference, and only blobs have one.
        let chunked = assemble(Kind::Blob, id, CODEC_CHUNKED, 16, None, b"list").unwrap();
        assert_eq!(chunked.header.header_len(), 52);
        assert_eq!(check(&chunked.bytes).unwrap().stored, b"list");
        let mut tree = chunked.bytes.clone();
        tree[4] = Kind::Tree.code();
        assert_eq!(parse_header(&tree), Err("only blobs are stored in chunks"));
    }

    #[test]
    fn every_flipped_byte_is_caught() {
        for payload in [&b"hello\n"[..], &b"compress me please ".repeat(20)] {
            let encoded = round_trip(Kind::Blob, payload);
            let checked = encoded.header.unpadded_len() as usize;
            for i in 0..checked {
                let mut damaged = encoded.bytes.clone();
                damaged[i] ^= 0x01;
                let caught = match check(&damaged) {
                    Err(_) => true,
                    Ok(found) => decode(&found.header, found.stored).is_err(),
                };
                assert!(caught, "flip at byte {i} went unnoticed");
            }
        }
    }

    #[test]
    fn cut_records_are_rejected() {
        let encoded = round_trip(Kind::Blob, b"hello\n");
        for len in 0..encoded.header.unpadded_len() as usize {
            assert!(check(&encoded.bytes[..len]).is_err(), "{len}");
        }
    }

    #[test]
    fn wrong_id_is_rejected_after_decoding() {
        let payload = b"some content";
        let wrong = ObjectId::hash(Kind::Blob, b"other content");
        let encoded = encode(Kind::Blob, wrong, payload).unwrap();
        let checked = check(&encoded.bytes).unwrap();
        assert_eq!(
            decode(&checked.header, checked.stored),
            Err("content does not hash to its id")
        );
    }

    #[test]
    fn reserved_codecs_parse_but_do_not_decode() {
        let mut encoded = round_trip(Kind::Blob, b"x").bytes;
        encoded[5] = CODEC_DICTIONARY;
        let header = parse_header(&encoded).unwrap();
        assert_eq!(header.header_len(), HEADER_LEN as u64 + 32);
        encoded[5] = 5;
        assert_eq!(parse_header(&encoded), Err("unknown codec"));
    }

    #[test]
    fn copies_rank_whole_first() {
        assert!(rank(CODEC_RAW) == rank(CODEC_ZSTD));
        assert!(rank(CODEC_ZSTD) < rank(CODEC_DELTA));
        assert!(rank(CODEC_DELTA) < rank(CODEC_CHUNKED));
        assert!(rank(CODEC_CHUNKED) < rank(CODEC_DICTIONARY));
        assert!(is_whole(CODEC_RAW) && is_whole(CODEC_ZSTD));
        assert!(!is_whole(CODEC_DELTA) && !is_whole(CODEC_CHUNKED));
    }
}
