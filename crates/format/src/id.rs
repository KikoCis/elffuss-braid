// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Object ids: BLAKE3 in key-derivation mode, with one context string per kind.
//!
//! Hashing under a per-kind context gives a blob and a tree with the same bytes
//! different ids, without a type prefix inside the payload, so a payload can be
//! hashed while it streams in. A blob id is always the hash of the blob's full
//! content, however the store happens to keep it (whole, delta, or chunked).

use core::fmt;
use core::str::FromStr;

/// What an object is. The numeric code is part of the on-disk format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Kind {
    /// File contents.
    Blob = 1,
    /// A directory listing: a flat directory, or one node of a large one.
    Tree = 2,
    /// One change to the repository: parents, root tree and per-file changes.
    Changeset = 3,
    /// The list of chunks a large blob is stored as.
    ChunkList = 4,
    /// A value computed from other objects, such as an index or a mapping.
    Derived = 5,
}

impl Kind {
    /// Every kind, in code order.
    pub const ALL: [Kind; 5] = [
        Kind::Blob,
        Kind::Tree,
        Kind::Changeset,
        Kind::ChunkList,
        Kind::Derived,
    ];

    /// The BLAKE3 key-derivation context for this kind. Changing one of these
    /// strings changes every id of that kind, so they are frozen with format v1.
    pub const fn context(self) -> &'static str {
        match self {
            Kind::Blob => "elffuss-braid 2026 blob v1",
            Kind::Tree => "elffuss-braid 2026 tree v1",
            Kind::Changeset => "elffuss-braid 2026 changeset v1",
            Kind::ChunkList => "elffuss-braid 2026 chunklist v1",
            Kind::Derived => "elffuss-braid 2026 derived v1",
        }
    }

    /// The one-byte code stored in object records.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The kind with this record code, if there is one.
    pub const fn from_code(code: u8) -> Option<Kind> {
        match code {
            1 => Some(Kind::Blob),
            2 => Some(Kind::Tree),
            3 => Some(Kind::Changeset),
            4 => Some(Kind::ChunkList),
            5 => Some(Kind::Derived),
            _ => None,
        }
    }

    /// The lowercase name used on the command line and in JSON output.
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Blob => "blob",
            Kind::Tree => "tree",
            Kind::Changeset => "changeset",
            Kind::ChunkList => "chunklist",
            Kind::Derived => "derived",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A 32-byte object id. Its text form is 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    /// Length of an id in bytes.
    pub const LEN: usize = 32;

    /// Number of hex digits in the short form shown to people.
    pub const SHORT_HEX: usize = 12;

    /// Wraps raw id bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw id bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Hashes a complete payload as an object of `kind`.
    pub fn hash(kind: Kind, payload: &[u8]) -> Self {
        Self(blake3::derive_key(kind.context(), payload))
    }

    /// The full text form: 64 lowercase hex digits.
    pub fn to_hex(&self) -> String {
        self.to_string()
    }

    /// The first [`Self::SHORT_HEX`] hex digits.
    pub fn short(&self) -> String {
        let mut text = self.to_hex();
        text.truncate(Self::SHORT_HEX);
        text
    }
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = [0u8; 64];
        for (i, byte) in self.0.iter().enumerate() {
            text[2 * i] = HEX_DIGITS[usize::from(byte >> 4)];
            text[2 * i + 1] = HEX_DIGITS[usize::from(byte & 0x0f)];
        }
        // Every byte written above is an ASCII hex digit.
        f.write_str(core::str::from_utf8(&text).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

/// Why a string is not an object id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseIdError {
    /// The text is not 64 characters long.
    Length(usize),
    /// The character at this byte position is not a lowercase hex digit.
    Digit(usize),
}

impl fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseIdError::Length(len) => {
                write!(
                    f,
                    "an object id has 64 hex digits, this has {len} characters"
                )
            }
            ParseIdError::Digit(pos) => {
                write!(f, "character {pos} is not a lowercase hex digit")
            }
        }
    }
}

impl std::error::Error for ParseIdError {}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

impl FromStr for ObjectId {
    type Err = ParseIdError;

    /// Parses the full 64-digit form. Only lowercase is accepted, so two equal
    /// ids always have equal text.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let digits = text.as_bytes();
        if digits.len() != 64 {
            return Err(ParseIdError::Length(digits.len()));
        }
        let mut bytes = [0u8; 32];
        for (i, pair) in digits.chunks_exact(2).enumerate() {
            let high = hex_value(pair[0]).ok_or(ParseIdError::Digit(2 * i))?;
            let low = hex_value(pair[1]).ok_or(ParseIdError::Digit(2 * i + 1))?;
            bytes[i] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

/// Incremental hashing, for payloads that arrive in pieces.
#[derive(Clone)]
pub struct IdHasher {
    inner: blake3::Hasher,
}

impl IdHasher {
    /// Starts hashing an object of `kind`.
    pub fn new(kind: Kind) -> Self {
        Self {
            inner: blake3::Hasher::new_derive_key(kind.context()),
        }
    }

    /// Feeds the next piece of the payload.
    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.inner.update(bytes);
        self
    }

    /// The id of everything fed so far. The hasher can keep going afterwards.
    pub fn finalize(&self) -> ObjectId {
        ObjectId(*self.inner.finalize().as_bytes())
    }
}

impl fmt::Debug for IdHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdHasher").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vectors. Each one can be checked independently with the `b3sum`
    /// tool: `printf 'hello\n' | b3sum --derive-key "elffuss-braid 2026 blob v1"`.
    const GOLDEN: &[(Kind, &[u8], &str)] = &[
        (
            Kind::Blob,
            b"",
            "ff9c212a7ef64fa38f0c411b3a68d523a20fd197ee3ba03fa4a44b7c5e99c958",
        ),
        (
            Kind::Blob,
            b"hello\n",
            "6acd5cdf880a325d2df716699943aae586ad77d54fb2ed0c5a3f53c76fa56f89",
        ),
        (
            Kind::Tree,
            b"",
            "46f1ab000b3145d2e28ff69f7570eb88065d76ef86905bb268699adb9c860c08",
        ),
        (
            Kind::Changeset,
            b"",
            "1be995db81f244ac5ea57f1ba45d9f9da94ad4edd6dbfa0979c16101a34d04ef",
        ),
        (
            Kind::ChunkList,
            b"",
            "f14231be43c0cc73e7c4850ec9d980e6952b56c57f7125d0c39d16874be2ab04",
        ),
        (
            Kind::Derived,
            b"",
            "6f101e23c531e6e3959878774beefb475944b2f0200e52eafb37fd80f97df172",
        ),
    ];

    #[test]
    fn golden_vectors() {
        for (kind, payload, expected) in GOLDEN {
            assert_eq!(
                ObjectId::hash(*kind, payload).to_hex(),
                *expected,
                "{kind} {payload:?}"
            );
        }
    }

    #[test]
    fn kinds_never_share_an_id() {
        for payload in [&b""[..], b"hello\n", &[0u8; 1000]] {
            let mut ids: Vec<ObjectId> = Kind::ALL
                .iter()
                .map(|kind| ObjectId::hash(*kind, payload))
                .collect();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), Kind::ALL.len());
        }
    }

    #[test]
    fn streaming_matches_one_shot() {
        // Long enough to cross BLAKE3's 1 KiB chunk and SIMD batch boundaries.
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let whole = ObjectId::hash(Kind::Blob, &payload);
        for piece in [1, 7, 1024, 4096, 65_537] {
            let mut hasher = IdHasher::new(Kind::Blob);
            for part in payload.chunks(piece) {
                hasher.update(part);
            }
            assert_eq!(hasher.finalize(), whole, "pieces of {piece}");
        }
    }

    #[test]
    fn text_round_trip() {
        let id = ObjectId::hash(Kind::Blob, b"round trip");
        let text = id.to_hex();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<ObjectId>(), Ok(id));
        assert_eq!(id.short(), text[..ObjectId::SHORT_HEX]);
        assert_eq!(format!("{id:?}"), format!("ObjectId({text})"));
    }

    #[test]
    fn rejects_bad_text() {
        let good = ObjectId::hash(Kind::Blob, b"x").to_hex();
        assert_eq!("".parse::<ObjectId>(), Err(ParseIdError::Length(0)));
        assert_eq!(
            good[..63].parse::<ObjectId>(),
            Err(ParseIdError::Length(63))
        );
        let mut upper = good.clone().into_bytes();
        upper[0] = b'A';
        assert_eq!(
            String::from_utf8(upper).unwrap().parse::<ObjectId>(),
            Err(ParseIdError::Digit(0))
        );
        let mut bad = good.clone().into_bytes();
        bad[10] = b'g';
        assert_eq!(
            String::from_utf8(bad).unwrap().parse::<ObjectId>(),
            Err(ParseIdError::Digit(10))
        );
    }

    #[test]
    fn kind_codes_round_trip() {
        for kind in Kind::ALL {
            assert_eq!(Kind::from_code(kind.code()), Some(kind));
        }
        for code in [0u8, 6, 7, 127, 255] {
            assert_eq!(Kind::from_code(code), None);
        }
    }
}
