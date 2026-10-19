// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Tokens: `brd_<id>_<secret>`, with an 8-character id and a 52-character
//! secret (32 random bytes), both lowercase base32 without padding.

use std::fmt;

/// The prefix every token starts with.
pub const PREFIX: &str = "brd_";
/// Characters in a token's id.
pub const ID_LEN: usize = 8;
/// Characters in a token's secret: 32 bytes in base32.
pub const SECRET_LEN: usize = 52;
/// Random bytes a token's id is made from.
pub const ID_BYTES: usize = 5;
/// Random bytes a token's secret is made from.
pub const SECRET_BYTES: usize = 32;

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Why a string is not a token.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// It does not have the shape of one.
    #[error(
        "not a braid hub token: expected brd_ then an 8-character id, _ and a 52-character secret"
    )]
    Shape,
}

/// A token. Its [`Debug`](fmt::Debug) form shows the id only.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    id: String,
    secret: String,
}

impl Token {
    /// A token from random bytes the caller drew from the operating system.
    pub fn from_random(id: [u8; ID_BYTES], secret: [u8; SECRET_BYTES]) -> Token {
        Token {
            id: base32(&id),
            secret: base32(&secret),
        }
    }

    /// Reads a token, rejecting anything not exactly of its shape.
    pub fn parse(text: &str) -> Result<Token, TokenError> {
        let rest = text.strip_prefix(PREFIX).ok_or(TokenError::Shape)?;
        let (id, secret) = rest.split_once('_').ok_or(TokenError::Shape)?;
        if !is_id(id) || !is_base32(secret, SECRET_LEN) {
            return Err(TokenError::Shape);
        }
        Ok(Token {
            id: id.to_owned(),
            secret: secret.to_owned(),
        })
    }

    /// The public id, which logs and lists name.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The whole token, `brd_<id>_<secret>`: what a client sends. Never log
    /// it.
    pub fn expose(&self) -> String {
        format!("{PREFIX}{}_{}", self.id, self.secret)
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Token({PREFIX}{}_…)", self.id)
    }
}

/// Whether `text` has the shape of a token's id, as lists and
/// `DELETE /v1/tokens/{id}` name it: 8 characters of lowercase base32.
pub fn is_id(text: &str) -> bool {
    is_base32(text, ID_LEN)
}

fn is_base32(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| ALPHABET.contains(&b))
}

/// Lowercase RFC 4648 base32, without padding.
pub fn base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u64 = 0;
    let mut bits = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | u64::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_matches_rfc_4648() {
        // RFC 4648, section 10, lowercased and without padding.
        for (input, expected) in [
            ("", ""),
            ("f", "my"),
            ("fo", "mzxq"),
            ("foo", "mzxw6"),
            ("foob", "mzxw6yq"),
            ("fooba", "mzxw6ytb"),
            ("foobar", "mzxw6ytboi"),
        ] {
            assert_eq!(base32(input.as_bytes()), expected);
        }
    }

    #[test]
    fn round_trip_and_shape() {
        let token = Token::from_random([1; ID_BYTES], [7; SECRET_BYTES]);
        let text = token.expose();
        assert!(text.starts_with("brd_"));
        assert_eq!(text.len(), 4 + ID_LEN + 1 + SECRET_LEN);
        assert_eq!(Token::parse(&text), Ok(token.clone()));
        assert!(!format!("{token:?}").contains(&text[13..]));
        assert!(is_id(token.id()));
        assert!(!is_id("abcdefg") && !is_id("abcdefgh1") && !is_id("ABCDEFGH"));
        for bad in [
            "",
            "brd_",
            &text[1..],
            &text.replace("brd_", "brx_"),
            &text.to_uppercase(),
            &format!("{text}a"),
            &text.replacen('_', "-", 2),
        ] {
            assert_eq!(Token::parse(bad), Err(TokenError::Shape), "{bad}");
        }
    }
}
