// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The canonical binary codec shared by every hashed format.
//!
//! - `uv`: unsigned LEB128, at most 64 bits, always in its shortest form.
//! - `sv`: a signed integer, zigzag-mapped and then written as `uv`.
//! - `bytes`: a `uv` length followed by that many raw bytes.
//!
//! The decoder is strict: it rejects overlong integers, integers that do not
//! fit in 64 bits, lengths that run past the input, and trailing bytes. Every
//! value therefore has exactly one encoding, which is what lets an id stand for
//! the value.

use core::fmt;

use crate::id::ObjectId;

/// Why some bytes are not a canonical encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The input ended in the middle of a field that starts at `at`.
    UnexpectedEnd { at: usize },
    /// The integer at `at` uses more bytes than its shortest form.
    NonMinimalInteger { at: usize },
    /// The integer at `at` does not fit in 64 bits.
    IntegerOverflow { at: usize },
    /// The length or count at `at` is larger than the rest of the input.
    LengthTooLarge { at: usize },
    /// Bytes were left after the last field, starting at `at`.
    TrailingBytes { at: usize },
    /// The field at `at` holds a value the format does not allow.
    Invalid { at: usize, what: &'static str },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnexpectedEnd { at } => {
                write!(f, "input ends inside the field at byte {at}")
            }
            DecodeError::NonMinimalInteger { at } => {
                write!(f, "integer at byte {at} is not in its shortest form")
            }
            DecodeError::IntegerOverflow { at } => {
                write!(f, "integer at byte {at} does not fit in 64 bits")
            }
            DecodeError::LengthTooLarge { at } => {
                write!(f, "length at byte {at} runs past the end of the input")
            }
            DecodeError::TrailingBytes { at } => write!(f, "unexpected bytes from byte {at} on"),
            DecodeError::Invalid { at, what } => write!(f, "invalid {what} at byte {at}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Reads canonical fields from a byte slice, front to back.
#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Starts reading at the beginning of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Byte offset of the next field.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not read yet.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Reads exactly `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.remaining() {
            return Err(DecodeError::UnexpectedEnd { at: self.pos });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Reads one byte.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    /// Reads a fixed-size array.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    /// Reads a 32-byte object id.
    pub fn id(&mut self) -> Result<ObjectId, DecodeError> {
        Ok(ObjectId::from_bytes(self.array()?))
    }

    /// Reads an unsigned LEB128 integer in its shortest form.
    pub fn uv(&mut self) -> Result<u64, DecodeError> {
        let at = self.pos;
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8().map_err(|_| DecodeError::UnexpectedEnd { at })?;
            let low = u64::from(byte & 0x7f);
            if shift == 63 && low > 1 {
                return Err(DecodeError::IntegerOverflow { at });
            }
            value |= low << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 0 {
                    return Err(DecodeError::NonMinimalInteger { at });
                }
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                return Err(DecodeError::IntegerOverflow { at });
            }
        }
    }

    /// Reads a zigzag-mapped signed integer.
    pub fn sv(&mut self) -> Result<i64, DecodeError> {
        Ok(unzigzag(self.uv()?))
    }

    /// Reads a `uv` that must be a length or count, and checks that at least
    /// `count × min_item_bytes` bytes remain, so that a forged count can never
    /// make the decoder reserve more memory than the input could fill.
    pub fn count(&mut self, min_item_bytes: usize) -> Result<usize, DecodeError> {
        let at = self.pos;
        let n = self.uv()?;
        let n = usize::try_from(n).map_err(|_| DecodeError::LengthTooLarge { at })?;
        if n.saturating_mul(min_item_bytes) > self.remaining() {
            return Err(DecodeError::LengthTooLarge { at });
        }
        Ok(n)
    }

    /// Reads `bytes`: a length followed by that many bytes.
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let at = self.pos;
        let n = self.uv()?;
        let n = usize::try_from(n).map_err(|_| DecodeError::LengthTooLarge { at })?;
        if n > self.remaining() {
            return Err(DecodeError::LengthTooLarge { at });
        }
        self.take(n)
    }

    /// Succeeds only if every byte was read.
    pub fn finish(self) -> Result<(), DecodeError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes { at: self.pos })
        }
    }
}

/// Appends an unsigned LEB128 integer in its shortest form.
pub fn put_uv(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let low = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// Appends a zigzag-mapped signed integer.
pub fn put_sv(out: &mut Vec<u8>, value: i64) {
    put_uv(out, zigzag(value));
}

/// Appends `bytes`: the length, then the bytes.
pub fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_uv(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Appends a 32-byte object id.
pub fn put_id(out: &mut Vec<u8>, id: &ObjectId) {
    out.extend_from_slice(id.as_bytes());
}

/// Number of bytes `put_uv` writes for `value`.
pub fn uv_len(value: u64) -> usize {
    let bits = 64 - value.leading_zeros() as usize;
    bits.div_ceil(7).max(1)
}

/// Maps signed to unsigned so that small magnitudes stay small: 0, -1, 1, -2, …
/// become 0, 1, 2, 3, …
pub const fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// The inverse of [`zigzag`].
pub const fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uv_bytes(value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        put_uv(&mut out, value);
        out
    }

    fn read_uv(bytes: &[u8]) -> Result<u64, DecodeError> {
        let mut reader = Reader::new(bytes);
        let value = reader.uv()?;
        reader.finish()?;
        Ok(value)
    }

    #[test]
    fn uv_vectors() {
        let cases: &[(u64, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7f]),
            (128, &[0x80, 0x01]),
            (300, &[0xac, 0x02]),
            (16_383, &[0xff, 0x7f]),
            (16_384, &[0x80, 0x80, 0x01]),
            (
                u64::MAX,
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
            ),
        ];
        for (value, bytes) in cases {
            assert_eq!(uv_bytes(*value), *bytes, "{value}");
            assert_eq!(read_uv(bytes), Ok(*value), "{value}");
            assert_eq!(uv_len(*value), bytes.len(), "{value}");
        }
    }

    #[test]
    fn uv_rejects_non_canonical() {
        assert_eq!(
            read_uv(&[0x80, 0x00]),
            Err(DecodeError::NonMinimalInteger { at: 0 })
        );
        assert_eq!(
            read_uv(&[0xff, 0x80, 0x00]),
            Err(DecodeError::NonMinimalInteger { at: 0 })
        );
        // Eleven bytes, or a tenth byte above 1, cannot fit in 64 bits.
        assert_eq!(
            read_uv(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]),
            Err(DecodeError::IntegerOverflow { at: 0 })
        );
        assert_eq!(
            read_uv(&[
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x81, 0x00
            ]),
            Err(DecodeError::IntegerOverflow { at: 0 })
        );
        assert_eq!(read_uv(&[0x80]), Err(DecodeError::UnexpectedEnd { at: 0 }));
        assert_eq!(read_uv(&[]), Err(DecodeError::UnexpectedEnd { at: 0 }));
        assert_eq!(
            read_uv(&[0x01, 0x02]),
            Err(DecodeError::TrailingBytes { at: 1 })
        );
    }

    #[test]
    fn zigzag_vectors() {
        let cases: &[(i64, u64)] = &[
            (0, 0),
            (-1, 1),
            (1, 2),
            (-2, 3),
            (i64::MAX, u64::MAX - 1),
            (i64::MIN, u64::MAX),
        ];
        for (signed, unsigned) in cases {
            assert_eq!(zigzag(*signed), *unsigned);
            assert_eq!(unzigzag(*unsigned), *signed);
        }
    }

    #[test]
    fn bytes_and_count_guard_lengths() {
        let mut out = Vec::new();
        put_bytes(&mut out, b"abc");
        let mut reader = Reader::new(&out);
        assert_eq!(reader.bytes(), Ok(&b"abc"[..]));
        reader.finish().unwrap();

        // A length past the end of the input.
        let mut reader = Reader::new(&[0x05, b'a']);
        assert_eq!(reader.bytes(), Err(DecodeError::LengthTooLarge { at: 0 }));

        // A count that would need more bytes than remain.
        let mut forged = Vec::new();
        put_uv(&mut forged, u64::MAX);
        let mut reader = Reader::new(&forged);
        assert_eq!(reader.count(1), Err(DecodeError::LengthTooLarge { at: 0 }));
    }

    #[test]
    fn uv_round_trips_at_every_boundary() {
        for shift in 0..64 {
            for value in [(1u64 << shift) - 1, 1u64 << shift, (1u64 << shift) + 1] {
                assert_eq!(read_uv(&uv_bytes(value)), Ok(value));
                assert_eq!(uv_bytes(value).len(), uv_len(value));
            }
        }
    }
}
