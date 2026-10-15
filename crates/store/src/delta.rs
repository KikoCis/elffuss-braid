// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Deltas (codec 3): an object compressed with zstd against the raw payload
//! of another object, its base.
//!
//! The base is given to zstd as a reference prefix: the frame is an ordinary
//! zstd frame whose matches may reach back into the base, so decoding it
//! needs the base and nothing else. The window covers the base and the
//! object together, and long-distance matching finds the parts they share
//! however far apart they are.
//!
//! Depth is always one: a base is stored whole, never as a delta itself.
//! When the object a writer names as similar is a delta, the delta is made
//! against that delta's own base instead.

use braid_format::{Kind, ObjectId};
use zstd::zstd_safe::{CCtx, CParameter};

use crate::record::{self, Header, RAW_MAX, ZSTD_LEVEL};

/// Blobs smaller than this may be stored as deltas; larger ones are stored
/// in chunks instead. Trees may be deltas whatever their size.
pub(crate) const BLOB_MAX: u64 = crate::chunks::CHUNKED_MIN;

/// The largest window a delta may use: 128 MiB, the largest that zstd
/// decoders accept without being told to.
const MAX_WINDOW_LOG: u32 = 27;

/// The smallest window zstd has.
const MIN_WINDOW_LOG: u32 = 10;

/// Whether an object of `kind` and `len` bytes may be stored as a delta.
pub(crate) fn eligible(kind: Kind, len: usize) -> bool {
    // Small payloads are stored raw: a base id costs more than it saves.
    if len <= RAW_MAX {
        return false;
    }
    match kind {
        Kind::Tree => true,
        Kind::Blob => (len as u64) < BLOB_MAX,
        _ => false,
    }
}

/// The window, as a power of two, that holds `len` bytes, if zstd allows it.
fn window_log(len: usize) -> Option<u32> {
    let log = usize::BITS - len.saturating_sub(1).leading_zeros();
    (log <= MAX_WINDOW_LOG).then_some(log.max(MIN_WINDOW_LOG))
}

/// Compresses `payload` against `base`. `None` if the two do not fit in one
/// window or zstd fails; the caller then stores the payload on its own.
pub(crate) fn compress(payload: &[u8], base: &[u8]) -> Option<Vec<u8>> {
    let window = window_log(base.len().checked_add(payload.len())?)?;
    let mut context = CCtx::try_create()?;
    context
        .set_parameter(CParameter::CompressionLevel(ZSTD_LEVEL))
        .ok()?;
    context.set_parameter(CParameter::WindowLog(window)).ok()?;
    context
        .set_parameter(CParameter::EnableLongDistanceMatching(true))
        .ok()?;
    context.ref_prefix(base).ok()?;
    let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(payload.len()));
    context.compress2(&mut out, payload).ok()?;
    Some(out)
}

/// Restores the payload of a delta record from its stored frame and the raw
/// payload of its base, and verifies that it hashes to the record's id.
pub(crate) fn decode(header: &Header, stored: &[u8], base: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut decoder =
        zstd::stream::read::Decoder::with_ref_prefix(stored, base).map_err(|_| "bad zstd data")?;
    decoder
        .window_log_max(MAX_WINDOW_LOG)
        .map_err(|_| "bad zstd data")?;
    let payload = record::read_capped(decoder, header.raw_len)?;
    record::verify(header, &payload)?;
    Ok(payload)
}

/// The record for `payload` as a delta against `base`, if it is worth it:
/// the frame and the base id together must take at most half the bytes of
/// the payload stored on its own (`standalone_len`).
pub(crate) fn encode(
    kind: Kind,
    id: ObjectId,
    payload: &[u8],
    base_id: &ObjectId,
    base: &[u8],
    standalone_len: usize,
) -> Option<record::Encoded> {
    let frame = compress(payload, base)?;
    let cost = frame.len() + record::REFERENCE_LEN;
    if cost.saturating_mul(2) > standalone_len {
        return None;
    }
    record::assemble(
        kind,
        id,
        record::CODEC_DELTA,
        payload.len() as u64,
        Some(base_id),
        &frame,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{CODEC_DELTA, check, standalone};

    /// Text that compresses only a little on its own, as source code does.
    fn text(seed: u64, lines: usize) -> Vec<u8> {
        let mut state = seed | 1;
        let mut out = Vec::new();
        for i in 0..lines {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(format!("line {i}: value {state:016x}\n").as_bytes());
        }
        out
    }

    #[test]
    fn a_small_edit_is_a_small_delta() {
        let base = text(7, 2000);
        let mut edited = base.clone();
        edited[30_000..30_010].copy_from_slice(b"EDITED....");
        edited.extend_from_slice(b"one more line\n");
        let id = ObjectId::hash(Kind::Blob, &edited);
        let base_id = ObjectId::hash(Kind::Blob, &base);
        let (_, alone) = standalone(&edited);
        let encoded = encode(Kind::Blob, id, &edited, &base_id, &base, alone.len()).unwrap();
        assert_eq!(encoded.header.codec, CODEC_DELTA);
        assert!(
            (encoded.header.stored_len as usize) * 20 < alone.len(),
            "{} against {}",
            encoded.header.stored_len,
            alone.len()
        );
        let checked = check(&encoded.bytes).unwrap();
        assert_eq!(checked.reference, Some(base_id));
        assert_eq!(
            decode(&checked.header, checked.stored, &base).unwrap(),
            edited
        );
        // Another base gives a wrong payload, which the id check catches.
        assert!(decode(&checked.header, checked.stored, &text(8, 2000)).is_err());
    }

    #[test]
    fn unrelated_content_is_not_worth_a_delta() {
        let payload = text(1, 500);
        let base = text(2, 500);
        let id = ObjectId::hash(Kind::Blob, &payload);
        let (_, alone) = standalone(&payload);
        assert!(
            encode(
                Kind::Blob,
                id,
                &payload,
                &ObjectId::hash(Kind::Blob, &base),
                &base,
                alone.len()
            )
            .is_none()
        );
    }

    #[test]
    fn windows_cover_base_and_payload() {
        assert_eq!(window_log(0), Some(MIN_WINDOW_LOG));
        assert_eq!(window_log(1024), Some(10));
        assert_eq!(window_log(1025), Some(11));
        assert_eq!(window_log(8 << 20), Some(23));
        assert_eq!(window_log(1 << 27), Some(27));
        assert_eq!(window_log((1 << 27) + 1), None);
    }

    #[test]
    fn who_may_be_a_delta() {
        assert!(!eligible(Kind::Blob, RAW_MAX));
        assert!(eligible(Kind::Blob, RAW_MAX + 1));
        assert!(eligible(Kind::Blob, BLOB_MAX as usize - 1));
        assert!(!eligible(Kind::Blob, BLOB_MAX as usize));
        assert!(eligible(Kind::Tree, 10 << 20));
        assert!(!eligible(Kind::Changeset, 1000));
        assert!(!eligible(Kind::Derived, 1000));
    }
}
