// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Browsing a repository without cloning it: directories, files, READMEs (section 15).
//!
//! A revision (`ref`) is a branch, a tag (`tags/<name>`), a changeset's
//! full id, or the first 12 to 63 hex digits of the id of a changeset the
//! repository holds; the default branch when none is given.
//!
//! Names and paths come as UTF-8, with what is not UTF-8 replaced. One that
//! is not UTF-8 also comes in `raw_name` or `raw_path`, percent-encoded as
//! a route's path takes it ([`encode_path`]), so that a client can ask for
//! it again.

use braid_format::ObjectId;
use serde::{Deserialize, Serialize};

use crate::hex;

/// One entry of a directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    /// Its name.
    pub name: String,
    /// Its name percent-encoded, when it is not UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_name: Option<String>,
    /// `file`, `executable`, `symlink`, `tree` or `gitlink`.
    pub kind: String,
    /// Its blob or tree, or a gitlink's git commit, in hex.
    pub id: String,
    /// Its size in bytes, for a file, an executable or a symlink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// A page of a directory, in `GET …/contents/{path}`: directories first,
/// then the rest, each in order of their names' bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directory {
    /// The branch or tag the revision named; `None` for an id.
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    /// The changeset read.
    #[serde(with = "hex::id")]
    pub changeset: ObjectId,
    /// The directory, from the root, which is the empty path.
    pub path: String,
    /// The path percent-encoded, when it is not UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_path: Option<String>,
    /// The entries of this page.
    pub entries: Vec<DirEntry>,
    /// The cursor of the next page (`?after=`), or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// A file, a symlink or a gitlink, in `GET …/contents/{path}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    /// The branch or tag the revision named; `None` for an id.
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    /// The changeset read.
    #[serde(with = "hex::id")]
    pub changeset: ObjectId,
    /// Its path, from the root.
    pub path: String,
    /// Its path percent-encoded, when it is not UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_path: Option<String>,
    /// `file`, `executable`, `symlink` or `gitlink`.
    pub kind: String,
    /// Its blob, or a gitlink's git commit, in hex.
    pub id: String,
    /// Its size in bytes; `None` for a gitlink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Whether its content is binary: a NUL among its first 8,000 bytes.
    pub binary: bool,
    /// Its content, for a file or an executable that is not binary, is
    /// UTF-8, and is no larger than the hub shows as text (`text_view` of
    /// `limits.social`); `None` otherwise, or when `text=false` was asked.
    pub text: Option<String>,
    /// A symlink's target, which the hub never follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// A gitlink's git commit, in hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// `GET …/contents` and `GET …/contents/{path}`: what is at a path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Contents {
    /// A directory: `"type": "dir"`.
    Dir(Directory),
    /// Anything else: `"type": "file"`.
    File(File),
}

/// `GET …/readme`: the README of a directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readme {
    /// Its path, from the root.
    pub path: String,
    /// Its path percent-encoded, when it is not UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_path: Option<String>,
    /// Its blob.
    #[serde(with = "hex::id")]
    pub blob: ObjectId,
    /// Its size in bytes.
    pub size: u64,
    /// Its text, as UTF-8, cut at the largest README the hub renders.
    pub markdown: String,
    /// Its text rendered as HTML, safe to show: Markdown for a `.md` or
    /// `.markdown` file, preformatted text otherwise.
    pub html: String,
    /// Whether `markdown` was cut.
    pub truncated: bool,
}

/// `path`, percent-encoded as a route's path takes it: `/` and most of the
/// characters a path may hold as they are (letters, digits, `-._~` and
/// `!$()*+,;=:@`), every other byte as `%XX`. `&` and `'` are encoded too,
/// so that the result reads the same inside an HTML attribute, where `&`
/// would start a character reference.
pub fn encode_path(path: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(path.len());
    for &byte in path {
        let plain = byte.is_ascii_alphanumeric() || b"/-._~!$()*+,;=:@".contains(&byte);
        if plain {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

/// The text of `bytes`, and their percent-encoded form when they are not
/// UTF-8: a name or a path, as the answers of this section give it.
pub fn text_and_raw(bytes: &[u8]) -> (String, Option<String>) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), None),
        Err(_) => (
            String::from_utf8_lossy(bytes).into_owned(),
            Some(encode_path(bytes)),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::Kind;

    fn id(text: &str) -> ObjectId {
        ObjectId::hash(Kind::Changeset, text.as_bytes())
    }

    #[test]
    fn contents_on_the_wire() {
        let dir = Contents::Dir(Directory {
            reference: Some("main".into()),
            changeset: id("head"),
            path: String::new(),
            raw_path: None,
            entries: vec![DirEntry {
                name: "src".into(),
                raw_name: None,
                kind: "tree".into(),
                id: id("tree").to_hex(),
                size: None,
            }],
            next: Some("cursor".into()),
        });
        let text = serde_json::to_string(&dir).unwrap();
        assert!(text.starts_with(r#"{"type":"dir","ref":"main""#), "{text}");
        assert!(!text.contains("raw_") && !text.contains("size"), "{text}");
        assert_eq!(serde_json::from_str::<Contents>(&text).unwrap(), dir);

        let file = Contents::File(File {
            reference: None,
            changeset: id("head"),
            path: "a.txt".into(),
            raw_path: None,
            kind: "file".into(),
            id: id("blob").to_hex(),
            size: Some(2),
            binary: false,
            text: Some("a\n".into()),
            target: None,
            commit: None,
        });
        let text = serde_json::to_string(&file).unwrap();
        assert!(text.contains(r#""type":"file""#), "{text}");
        assert!(text.contains(r#""ref":null"#), "{text}");
        assert!(
            !text.contains("target") && !text.contains("commit"),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<Contents>(&text).unwrap(), file);

        let readme = Readme {
            path: "README.md".into(),
            raw_path: None,
            blob: id("readme"),
            size: 7,
            markdown: "# Hello".into(),
            html: "<h1>Hello</h1>".into(),
            truncated: false,
        };
        let text = serde_json::to_string(&readme).unwrap();
        assert_eq!(serde_json::from_str::<Readme>(&text).unwrap(), readme);
    }

    #[test]
    fn paths_are_encoded_as_routes_take_them() {
        assert_eq!(encode_path(b"src/main.rs"), "src/main.rs");
        assert_eq!(encode_path(b"a b/c#d?e%f"), "a%20b/c%23d%3Fe%25f");
        assert_eq!(encode_path(b"f\xff"), "f%FF");
        assert_eq!(encode_path("ñ".as_bytes()), "%C3%B1");
        assert_eq!(encode_path(b"(1)+x=y;@:~"), "(1)+x=y;@:~");
        assert_eq!(encode_path(b"<\"'>"), "%3C%22%27%3E");
        assert_eq!(encode_path(b"R&amp;D/a&lt;b"), "R%26amp;D/a%26lt;b");
        assert_eq!(text_and_raw(b"plain"), ("plain".to_owned(), None));
        let (text, raw) = text_and_raw(b"bad\xfe");
        assert_eq!(text, "bad\u{fffd}");
        assert_eq!(raw.as_deref(), Some("bad%FE"));
    }
}
