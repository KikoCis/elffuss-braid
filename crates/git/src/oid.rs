// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Git object ids and kinds.

use core::fmt;
use core::str::FromStr;

use braid_format::GitId;

use crate::error::Error;

/// The four kinds of git object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GitKind {
    /// A commit.
    Commit,
    /// A directory listing.
    Tree,
    /// File content, or the target of a symlink.
    Blob,
    /// An annotated tag.
    Tag,
}

impl GitKind {
    /// The name git writes in object headers: `commit`, `tree`, `blob` or `tag`.
    pub const fn name(self) -> &'static str {
        match self {
            GitKind::Commit => "commit",
            GitKind::Tree => "tree",
            GitKind::Blob => "blob",
            GitKind::Tag => "tag",
        }
    }

    /// The code used in serialized maps.
    pub(crate) const fn code(self) -> u8 {
        match self {
            GitKind::Commit => 1,
            GitKind::Tree => 2,
            GitKind::Blob => 3,
            GitKind::Tag => 4,
        }
    }

    /// The kind with this map code.
    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(GitKind::Commit),
            2 => Some(GitKind::Tree),
            3 => Some(GitKind::Blob),
            4 => Some(GitKind::Tag),
            _ => None,
        }
    }

    pub(crate) fn to_gix(self) -> gix::objs::Kind {
        match self {
            GitKind::Commit => gix::objs::Kind::Commit,
            GitKind::Tree => gix::objs::Kind::Tree,
            GitKind::Blob => gix::objs::Kind::Blob,
            GitKind::Tag => gix::objs::Kind::Tag,
        }
    }

    pub(crate) fn from_gix(kind: gix::objs::Kind) -> Self {
        match kind {
            gix::objs::Kind::Commit => GitKind::Commit,
            gix::objs::Kind::Tree => GitKind::Tree,
            gix::objs::Kind::Blob => GitKind::Blob,
            gix::objs::Kind::Tag => GitKind::Tag,
        }
    }
}

impl fmt::Display for GitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A git object id: the SHA-1 of the object's header and content. Its text
/// form is 40 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitOid([u8; 20]);

impl GitOid {
    /// Length of an id in bytes.
    pub const LEN: usize = 20;

    /// Wraps raw id bytes.
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// The raw id bytes.
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// The full text form: 40 lowercase hex digits.
    pub fn to_hex(&self) -> String {
        self.to_string()
    }

    /// The id git gives an object of `kind` holding `data`.
    ///
    /// The hash runs with SHA-1 collision detection, as git's does: data that
    /// carries a known collision attack gets an error instead of an id.
    pub fn for_object(kind: GitKind, data: &[u8]) -> Result<Self, Error> {
        let id = gix::objs::compute_hash(gix::hash::Kind::Sha1, kind.to_gix(), data)
            .map_err(|_| Error::Collision { kind })?;
        Ok(Self::from_gix(&id).expect("a SHA-1 hash has 20 bytes"))
    }

    /// The id, if `id` is a SHA-1 id.
    pub(crate) fn from_gix(id: &gix::oid) -> Option<Self> {
        <[u8; 20]>::try_from(id.as_bytes()).ok().map(Self)
    }

    pub(crate) fn to_gix(self) -> gix::ObjectId {
        gix::ObjectId::Sha1(self.0)
    }

    /// Parses 40 hex digits of either case, as found inside git objects.
    pub(crate) fn parse_hex(digits: &[u8]) -> Option<Self> {
        if digits.len() != 40 {
            return None;
        }
        let mut bytes = [0u8; 20];
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks_exact(2)) {
            *byte = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
        }
        Some(Self(bytes))
    }
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = [0u8; 40];
        for (i, byte) in self.0.iter().enumerate() {
            text[2 * i] = HEX_DIGITS[usize::from(byte >> 4)];
            text[2 * i + 1] = HEX_DIGITS[usize::from(byte & 0x0f)];
        }
        // Every byte written above is an ASCII hex digit.
        f.write_str(core::str::from_utf8(&text).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GitOid({self})")
    }
}

/// Why a string is not a git object id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseGitOidError;

impl fmt::Display for ParseGitOidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a git object id is 40 lowercase hex digits")
    }
}

impl std::error::Error for ParseGitOidError {}

impl FromStr for GitOid {
    type Err = ParseGitOidError;

    /// Parses the 40-digit form. Only lowercase is accepted, so two equal ids
    /// always have equal text.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(ParseGitOidError);
        }
        Self::parse_hex(text.as_bytes()).ok_or(ParseGitOidError)
    }
}

impl From<GitOid> for GitId {
    fn from(oid: GitOid) -> Self {
        GitId::Sha1(oid.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_ids() {
        // The empty blob and the empty tree have ids every git user has seen.
        assert_eq!(
            GitOid::for_object(GitKind::Blob, b"").unwrap().to_hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            GitOid::for_object(GitKind::Tree, b"").unwrap().to_hex(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
        assert_eq!(
            GitOid::for_object(GitKind::Blob, b"hello\n")
                .unwrap()
                .to_hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn text_round_trip() {
        let oid = GitOid::for_object(GitKind::Blob, b"x").unwrap();
        let text = oid.to_hex();
        assert_eq!(text.parse::<GitOid>(), Ok(oid));
        assert_eq!(format!("{oid:?}"), format!("GitOid({text})"));
        assert_eq!(text.to_uppercase().parse::<GitOid>(), Err(ParseGitOidError));
        assert_eq!(GitOid::parse_hex(text.to_uppercase().as_bytes()), Some(oid));
        assert_eq!(text[..39].parse::<GitOid>(), Err(ParseGitOidError));
        assert_eq!(GitOid::parse_hex(b"zz"), None);
    }

    #[test]
    fn kind_codes_round_trip() {
        for kind in [GitKind::Commit, GitKind::Tree, GitKind::Blob, GitKind::Tag] {
            assert_eq!(GitKind::from_code(kind.code()), Some(kind));
            assert_eq!(GitKind::from_gix(kind.to_gix()), kind);
        }
        assert_eq!(GitKind::from_code(0), None);
        assert_eq!(GitKind::from_code(5), None);
    }
}
