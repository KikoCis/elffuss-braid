// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Hex prefixes of object ids: what a short id given on the command line
//! names.

use braid_format::ObjectId;

/// The first hex digits of an id, as their values, one per digit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HexPrefix {
    digits: Vec<u8>,
}

impl HexPrefix {
    /// Reads lowercase hex digits, at most as many as a full id has. Anything
    /// else is not a prefix.
    pub(crate) fn parse(text: &str) -> Option<HexPrefix> {
        if text.len() > 2 * ObjectId::LEN {
            return None;
        }
        let digits = text
            .bytes()
            .map(|digit| match digit {
                b'0'..=b'9' => Some(digit - b'0'),
                b'a'..=b'f' => Some(digit - b'a' + 10),
                _ => None,
            })
            .collect::<Option<Vec<u8>>>()?;
        Some(HexPrefix { digits })
    }

    /// Whether the hex form of `id` starts with these digits.
    pub(crate) fn matches(&self, id: &ObjectId) -> bool {
        let bytes = id.as_bytes();
        self.digits.iter().enumerate().all(|(at, digit)| {
            let byte = bytes[at / 2];
            let found = if at % 2 == 0 { byte >> 4 } else { byte & 0x0f };
            found == *digit
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::Kind;

    #[test]
    fn prefixes_of_every_length_match() {
        let id = ObjectId::hash(Kind::Blob, b"hello\n");
        let hex = id.to_hex();
        for len in 0..=64 {
            let prefix = HexPrefix::parse(&hex[..len]).unwrap();
            assert!(prefix.matches(&id), "{len} digits");
        }
        let other = ObjectId::hash(Kind::Blob, b"other\n");
        let shared = hex
            .bytes()
            .zip(other.to_hex().bytes())
            .take_while(|(a, b)| a == b)
            .count();
        assert!(
            !HexPrefix::parse(&hex[..shared + 1])
                .unwrap()
                .matches(&other)
        );
    }

    #[test]
    fn only_lowercase_hex_digits_are_a_prefix() {
        for bad in ["ABC", "xyz", "12 4", "-1", &"0".repeat(65)] {
            assert_eq!(HexPrefix::parse(bad), None, "{bad:?}");
        }
        assert_eq!(HexPrefix::parse("").unwrap().digits, Vec::<u8>::new());
        assert_eq!(HexPrefix::parse("0af").unwrap().digits, [0, 10, 15]);
    }
}
