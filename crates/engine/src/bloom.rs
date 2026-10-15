// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Changed-path filters: Bloom filters of the paths a changeset changes and
//! of their parent directories, so that a walk over history skips, without
//! reading them, the changesets that cannot have touched a path.
//!
//! A filter with `n` entries has `ceil(10 n / 8)` bytes, so at least 10 bits
//! per entry, and sets 7 bits per entry: the parameters of git's changed-path
//! filters, for about one false positive in a hundred lookups. Its encoding,
//! versioned with the record that holds it, is `uv n` followed by the bytes.
//!
//! The bits of an entry come from `h = BLAKE3-derive-key("elffuss-braid 2026
//! changed-paths v1", path)`: with `a` and `b` its first two little-endian
//! 64-bit words and `m` the number of bits, entry bit `i`, for `i` from 0
//! to 6, is `(a + i b) mod m`, computed modulo 2⁶⁴; bit `j` of the filter is
//! bit `j mod 8` of byte `j / 8`. A filter never says no for an entry it
//! holds. An empty change list has no entries, and a filter that would hold
//! more than [`MAX_ENTRIES`] has no bits: every path may match.

use std::collections::HashSet;

use braid_format::DecodeError;
use braid_format::codec::{Reader, put_uv};

/// Bits per entry, at least.
const BITS_PER_ENTRY: u64 = 10;

/// Bits set per entry.
const HASHES: u64 = 7;

/// A changeset whose paths and directories are more than this many has no
/// filter: every path may match it.
pub(crate) const MAX_ENTRIES: u64 = 1024;

/// The key-derivation context of the entry hashes.
const CONTEXT: &str = "elffuss-braid 2026 changed-paths v1";

/// The hash of a path, from which the bits of every filter size come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PathHash {
    a: u64,
    b: u64,
}

impl PathHash {
    /// The hash of `path`.
    pub(crate) fn of(path: &[u8]) -> Self {
        let digest = blake3::derive_key(CONTEXT, path);
        let word = |at: usize| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&digest[at..at + 8]);
            u64::from_le_bytes(bytes)
        };
        PathHash {
            a: word(0),
            b: word(8),
        }
    }

    /// The bits of the entry in a filter of `bits` bits.
    fn positions(self, bits: u64) -> impl Iterator<Item = u64> {
        (0..HASHES).map(move |i| self.a.wrapping_add(i.wrapping_mul(self.b)) % bits)
    }
}

/// The changed-path filter of one changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Filter {
    /// The change list is empty: no path matches.
    Empty,
    /// A Bloom filter of `entries` entries.
    Bits {
        /// How many paths and directories it holds.
        entries: u64,
        /// The bits, `ceil(10 × entries / 8)` bytes of them.
        bits: Vec<u8>,
    },
    /// More than [`MAX_ENTRIES`] entries: every path may match.
    Full {
        /// How many paths and directories there are.
        entries: u64,
    },
}

impl Filter {
    /// The filter of a change list with these paths: the paths and all of
    /// their parent directories.
    pub(crate) fn build<'p>(paths: impl IntoIterator<Item = &'p [u8]>) -> Filter {
        let mut entries: HashSet<&[u8]> = HashSet::new();
        for path in paths {
            if !entries.insert(path) {
                continue;
            }
            // Every entry already holds its own directories, so the walk up
            // stops at the first one already there.
            let mut end = path.len();
            while let Some(slash) = path[..end].iter().rposition(|byte| *byte == b'/') {
                if !entries.insert(&path[..slash]) {
                    break;
                }
                end = slash;
            }
        }
        let count = entries.len() as u64;
        if count == 0 {
            return Filter::Empty;
        }
        if count > MAX_ENTRIES {
            return Filter::Full { entries: count };
        }
        let mut bits = vec![0u8; filter_bytes(count)];
        let size = bits.len() as u64 * 8;
        for entry in entries {
            for bit in PathHash::of(entry).positions(size) {
                bits[(bit / 8) as usize] |= 1 << (bit % 8);
            }
        }
        Filter::Bits {
            entries: count,
            bits,
        }
    }

    /// Whether the entry with this hash may be in the filter. `false` is
    /// certain; `true` is wrong about once in a hundred times.
    pub(crate) fn may_contain(&self, hash: PathHash) -> bool {
        match self {
            Filter::Empty => false,
            Filter::Full { .. } => true,
            Filter::Bits { bits, .. } => hash
                .positions(bits.len() as u64 * 8)
                .all(|bit| bits[(bit / 8) as usize] & (1 << (bit % 8)) != 0),
        }
    }

    /// Whether the change list is empty.
    pub(crate) fn is_empty(&self) -> bool {
        matches!(self, Filter::Empty)
    }

    /// Appends the encoding: the number of entries, then the bits when there
    /// are some.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Filter::Empty => put_uv(out, 0),
            Filter::Bits { entries, bits } => {
                put_uv(out, *entries);
                out.extend_from_slice(bits);
            }
            Filter::Full { entries } => put_uv(out, *entries),
        }
    }

    /// Reads an encoding written by [`Filter::encode`].
    pub(crate) fn decode(reader: &mut Reader<'_>) -> Result<Filter, DecodeError> {
        let entries = reader.uv()?;
        Ok(if entries == 0 {
            Filter::Empty
        } else if entries > MAX_ENTRIES {
            Filter::Full { entries }
        } else {
            Filter::Bits {
                entries,
                bits: reader.take(filter_bytes(entries))?.to_vec(),
            }
        })
    }
}

/// Bytes of the filter of `entries` entries.
fn filter_bytes(entries: u64) -> usize {
    (entries * BITS_PER_ENTRY).div_ceil(8) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn encoded(filter: &Filter) -> Vec<u8> {
        let mut out = Vec::new();
        filter.encode(&mut out);
        out
    }

    fn decoded(bytes: &[u8]) -> Result<Filter, DecodeError> {
        let mut reader = Reader::new(bytes);
        let filter = Filter::decode(&mut reader)?;
        reader.finish()?;
        Ok(filter)
    }

    #[test]
    fn paths_and_their_directories_are_entries() {
        let filter = Filter::build([&b"a/b/c.txt"[..], b"a/b/d.txt", b"e"]);
        // a/b/c.txt, a/b/d.txt, a/b, a, e.
        let Filter::Bits { entries, bits } = &filter else {
            panic!("expected bits, got {filter:?}");
        };
        assert_eq!(*entries, 5);
        assert_eq!(bits.len(), 7);
        for entry in [&b"a/b/c.txt"[..], b"a/b/d.txt", b"a/b", b"a", b"e"] {
            assert!(filter.may_contain(PathHash::of(entry)), "{entry:?}");
        }
        // Not certain, but these hashes are fixed: a test that fails here
        // means that the hash changed.
        assert!(!filter.may_contain(PathHash::of(b"a/b/c")));
        assert!(!filter.may_contain(PathHash::of(b"a/x")));
    }

    #[test]
    fn empty_and_full_filters() {
        let empty = Filter::build([]);
        assert!(empty.is_empty());
        assert!(!empty.may_contain(PathHash::of(b"")));
        assert_eq!(encoded(&empty), [0]);
        assert_eq!(decoded(&[0]), Ok(Filter::Empty));

        let names: Vec<Vec<u8>> = (0..MAX_ENTRIES)
            .map(|i| format!("f{i}").into_bytes())
            .collect();
        let at_limit = Filter::build(names.iter().map(Vec::as_slice));
        assert!(matches!(
            at_limit,
            Filter::Bits {
                entries: MAX_ENTRIES,
                ..
            }
        ));
        // One more path, and its directory: over the limit.
        let mut over = names.clone();
        over.push(b"d/x".to_vec());
        let full = Filter::build(over.iter().map(Vec::as_slice));
        assert_eq!(
            full,
            Filter::Full {
                entries: MAX_ENTRIES + 2
            }
        );
        assert!(full.may_contain(PathHash::of(b"anything")));
        assert_eq!(decoded(&encoded(&full)), Ok(full));
        assert_eq!(decoded(&encoded(&at_limit)), Ok(at_limit));
    }

    #[test]
    fn the_encoding_has_exactly_the_bytes_of_its_entries() {
        let filter = Filter::build([&b"x"[..]]);
        let bytes = encoded(&filter);
        assert_eq!(bytes.len(), 1 + 2);
        assert_eq!(decoded(&bytes), Ok(filter));
        assert!(decoded(&bytes[..2]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decoded(&longer).is_err());
    }

    #[test]
    fn false_positives_are_rare() {
        let changed: Vec<Vec<u8>> = (0..200)
            .map(|i| format!("src/m{}/f{i}.rs", i % 7).into_bytes())
            .collect();
        let filter = Filter::build(changed.iter().map(Vec::as_slice));
        let wrong = (0..10_000)
            .filter(|i| filter.may_contain(PathHash::of(format!("other/g{i}").as_bytes())))
            .count();
        assert!(wrong < 300, "{wrong} false positives in 10000");
    }

    proptest! {
        #[test]
        fn no_false_negatives(
            paths in proptest::collection::vec("[a-c]{1,3}(/[a-c]{1,3}){0,3}", 0..80),
        ) {
            let filter = Filter::build(paths.iter().map(|p| p.as_bytes()));
            for path in &paths {
                let mut prefix = path.as_str();
                loop {
                    prop_assert!(filter.may_contain(PathHash::of(prefix.as_bytes())), "{prefix}");
                    match prefix.rfind('/') {
                        Some(slash) => prefix = &prefix[..slash],
                        None => break,
                    }
                }
            }
            prop_assert_eq!(decoded(&encoded(&filter)), Ok(filter));
        }
    }
}
