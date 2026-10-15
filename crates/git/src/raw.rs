// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The raw bytes of git objects: the few fields braid reads from them, and the
//! canonical form it writes.
//!
//! Braid keeps the original bytes of every imported commit and tag, and of
//! every tree it cannot regenerate exactly, so these parsers only need the
//! fields that link objects together and the fields a changeset records. They
//! are total where they can be: odd author lines or extra headers never stop an
//! import, because the original bytes travel with the changeset.
//!
//! Written from git's documented object format; no code comes from git.

use core::cmp::Ordering;

use braid_format::Signature;

use crate::oid::GitOid;

/// What a git tree entry is, by the file type bits of its mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryType {
    File,
    Executable,
    Symlink,
    Tree,
    Gitlink,
}

impl EntryType {
    /// The type of an entry with this mode, written in octal as in a tree.
    /// A regular file is executable when the owner may execute it, so a
    /// non-canonical mode such as `100664` still has a type; `None` means the
    /// file type bits name nothing git can store.
    pub(crate) fn from_mode(mode: &[u8]) -> Option<EntryType> {
        let value = parse_octal(mode)?;
        match value & 0o170_000 {
            0o100_000 if value & 0o100 != 0 => Some(EntryType::Executable),
            0o100_000 => Some(EntryType::File),
            0o120_000 => Some(EntryType::Symlink),
            0o040_000 => Some(EntryType::Tree),
            0o160_000 => Some(EntryType::Gitlink),
            _ => None,
        }
    }

    /// The mode git writes for this type.
    pub(crate) const fn canonical_mode(self) -> &'static [u8] {
        match self {
            EntryType::File => b"100644",
            EntryType::Executable => b"100755",
            EntryType::Symlink => b"120000",
            EntryType::Tree => b"40000",
            EntryType::Gitlink => b"160000",
        }
    }
}

fn parse_octal(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u32, |value, digit| match digit {
        b'0'..=b'7' => value.checked_mul(8)?.checked_add(u32::from(digit - b'0')),
        _ => None,
    })
}

/// One entry of a git tree, as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RawEntry<'a> {
    pub mode: &'a [u8],
    pub name: &'a [u8],
    pub oid: GitOid,
}

/// Splits a tree into its entries, in the order they are written.
pub(crate) fn parse_tree(data: &[u8]) -> Result<Vec<RawEntry<'_>>, &'static str> {
    let mut entries = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let space = find_byte(rest, b' ').ok_or("an entry has no space after its mode")?;
        let mode = &rest[..space];
        let after = &rest[space + 1..];
        let nul = find_byte(after, 0).ok_or("an entry name has no terminating NUL")?;
        let name = &after[..nul];
        let id: [u8; 20] = after
            .get(nul + 1..nul + 1 + GitOid::LEN)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or("an entry id is cut short")?;
        entries.push(RawEntry {
            mode,
            name,
            oid: GitOid::from_bytes(id),
        });
        rest = &after[nul + 1 + GitOid::LEN..];
    }
    Ok(entries)
}

/// One entry of a tree to write.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TreeItem<'a> {
    pub kind: EntryType,
    pub name: &'a [u8],
    pub oid: GitOid,
}

/// Writes a tree as git does: canonical modes, and entries sorted by name
/// bytes with a directory compared as if its name ended in `/`.
pub(crate) fn encode_tree(items: &mut [TreeItem<'_>]) -> Vec<u8> {
    items.sort_by(|a, b| {
        git_order(
            a.name,
            a.kind == EntryType::Tree,
            b.name,
            b.kind == EntryType::Tree,
        )
    });
    let size = items
        .iter()
        .map(|item| item.kind.canonical_mode().len() + item.name.len() + 2 + GitOid::LEN)
        .sum();
    let mut out = Vec::with_capacity(size);
    for item in items.iter() {
        out.extend_from_slice(item.kind.canonical_mode());
        out.push(b' ');
        out.extend_from_slice(item.name);
        out.push(0);
        out.extend_from_slice(item.oid.as_bytes());
    }
    out
}

/// Git's order of tree entries: by name bytes, with a directory's name
/// followed by `/`.
pub(crate) fn git_order(a: &[u8], a_dir: bool, b: &[u8], b_dir: bool) -> Ordering {
    let common = a.len().min(b.len());
    a[..common].cmp(&b[..common]).then_with(|| {
        let next = |name: &[u8], dir: bool| name.get(common).copied().or(dir.then_some(b'/'));
        next(a, a_dir).cmp(&next(b, b_dir))
    })
}

/// The fields of a commit that braid reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommitFields<'a> {
    pub tree: GitOid,
    pub parents: Vec<GitOid>,
    pub author: Option<&'a [u8]>,
    pub committer: Option<&'a [u8]>,
    pub message: &'a [u8],
}

/// Splits an object at its first empty line into the header lines (each one
/// ending in a newline) and the message.
fn split_message(data: &[u8]) -> (&[u8], &[u8]) {
    if data.first() == Some(&b'\n') {
        return (&[], &data[1..]);
    }
    match data.windows(2).position(|pair| pair == b"\n\n") {
        Some(end) => (&data[..end + 1], &data[end + 2..]),
        None => (data, &[]),
    }
}

/// The header lines as key and value. Continuation lines, which start with a
/// space and belong to a multi-line header such as a signature, are skipped.
fn header_lines(headers: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    headers
        .split(|byte| *byte == b'\n')
        .filter(|line| line.first().is_some_and(|byte| *byte != b' '))
        .map(|line| match find_byte(line, b' ') {
            Some(space) => (&line[..space], &line[space + 1..]),
            None => (line, &line[line.len()..]),
        })
}

/// Reads the tree, the parents, the author and committer lines and the
/// message of a commit.
///
/// The tree is the first header and the parents are the `parent` headers right
/// after it. The author and committer are the first headers with those names,
/// wherever they are; the message is everything after the first empty line.
pub(crate) fn parse_commit(data: &[u8]) -> Result<CommitFields<'_>, &'static str> {
    let (headers, message) = split_message(data);
    let mut lines = header_lines(headers).peekable();
    let tree = match lines.next() {
        Some((b"tree", value)) => {
            GitOid::parse_hex(value).ok_or("the tree header does not hold an object id")?
        }
        _ => return Err("the first header is not a tree"),
    };
    let mut parents = Vec::new();
    while let Some((b"parent", value)) = lines.peek() {
        parents.push(GitOid::parse_hex(value).ok_or("a parent header does not hold an object id")?);
        lines.next();
    }
    let mut author = None;
    let mut committer = None;
    for (key, value) in lines {
        match key {
            b"author" if author.is_none() => author = Some(value),
            b"committer" if committer.is_none() => committer = Some(value),
            _ => {}
        }
    }
    Ok(CommitFields {
        tree,
        parents,
        author,
        committer,
        message,
    })
}

/// Reads the object an annotated tag points to: its first header.
pub(crate) fn parse_tag(data: &[u8]) -> Result<GitOid, &'static str> {
    let (headers, _) = split_message(data);
    match header_lines(headers).next() {
        Some((b"object", value)) => {
            GitOid::parse_hex(value).ok_or("the object header does not hold an object id")
        }
        _ => Err("the first header is not an object"),
    }
}

/// Reads an author or committer line: `Name <email> seconds ±HHMM`.
///
/// The rule is fixed, because these fields are hashed into the changeset id:
///
/// - The name is everything before the first `<`, without surrounding ASCII
///   whitespace; the email is everything between it and the next `>`.
/// - After the `>`, the first word is the time in seconds and the second the
///   offset as `±HHMM`. A missing or unreadable value is 0.
/// - A line without `<` and a later `>` is all name, with no email and time 0.
///
/// Nothing is lost: the original line stays in the commit bytes.
pub(crate) fn parse_signature(line: &[u8]) -> Signature {
    let parts = find_byte(line, b'<').and_then(|open| {
        let close = open + 1 + find_byte(&line[open + 1..], b'>')?;
        Some((&line[..open], &line[open + 1..close], &line[close + 1..]))
    });
    let Some((name, email, rest)) = parts else {
        return Signature {
            name: line.trim_ascii().to_vec(),
            email: Vec::new(),
            seconds: 0,
            offset_minutes: 0,
        };
    };
    let mut words = rest
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|word| !word.is_empty());
    let seconds = words.next().and_then(parse_decimal).unwrap_or(0);
    let offset_minutes = words.next().and_then(parse_offset).unwrap_or(0);
    Signature {
        name: name.trim_ascii().to_vec(),
        email: email.to_vec(),
        seconds,
        offset_minutes,
    }
}

/// An optional sign followed by decimal digits, if it fits in an `i64`.
fn parse_decimal(word: &[u8]) -> Option<i64> {
    let (negative, digits) = match word.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, word),
    };
    if digits.is_empty() {
        return None;
    }
    let magnitude = digits.iter().try_fold(0i64, |value, digit| match digit {
        b'0'..=b'9' => value.checked_mul(10)?.checked_add(i64::from(digit - b'0')),
        _ => None,
    })?;
    Some(if negative { -magnitude } else { magnitude })
}

/// `±HHMM` as minutes.
fn parse_offset(word: &[u8]) -> Option<i64> {
    let value = parse_decimal(word)?;
    let minutes = (value.abs() / 100)
        .checked_mul(60)?
        .checked_add(value.abs() % 100)?;
    Some(if value < 0 { -minutes } else { minutes })
}

/// Appends `key Name <email> seconds ±HHMM` and a newline.
fn write_signature(out: &mut Vec<u8>, key: &[u8], signature: &Signature) -> Result<(), String> {
    let forbidden = |byte: &u8| matches!(byte, b'<' | b'>' | b'\n' | 0);
    if signature.name.iter().any(forbidden) {
        return Err("a name holds '<', '>', a newline or NUL".into());
    }
    if signature.email.iter().any(forbidden) {
        return Err("an email holds '<', '>', a newline or NUL".into());
    }
    let magnitude = signature.offset_minutes.unsigned_abs();
    if magnitude / 60 > 99 {
        return Err(format!(
            "the offset of {} minutes does not fit in ±HHMM",
            signature.offset_minutes
        ));
    }
    out.extend_from_slice(key);
    out.push(b' ');
    out.extend_from_slice(&signature.name);
    out.extend_from_slice(b" <");
    out.extend_from_slice(&signature.email);
    out.extend_from_slice(b"> ");
    out.extend_from_slice(signature.seconds.to_string().as_bytes());
    let sign = if signature.offset_minutes < 0 {
        '-'
    } else {
        '+'
    };
    let offset = format!(" {sign}{:02}{:02}\n", magnitude / 60, magnitude % 60);
    out.extend_from_slice(offset.as_bytes());
    Ok(())
}

/// The headers a commit generated by braid writes itself, which an extra must
/// not repeat.
const RESERVED_HEADERS: [&[u8]; 4] = [b"tree", b"parent", b"author", b"committer"];

/// Writes a commit for a changeset that did not come from git.
///
/// The headers are `tree`, `parent` for each parent, `author`, `committer`,
/// then one header per extra in order, a multi-line value continuing on lines
/// that start with a space; then an empty line and the message, unchanged.
pub(crate) fn encode_commit(
    tree: GitOid,
    parents: &[GitOid],
    author: &Signature,
    committer: &Signature,
    extras: &[(Vec<u8>, Vec<u8>)],
    message: &[u8],
) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(256 + message.len());
    out.extend_from_slice(b"tree ");
    out.extend_from_slice(tree.to_hex().as_bytes());
    out.push(b'\n');
    for parent in parents {
        out.extend_from_slice(b"parent ");
        out.extend_from_slice(parent.to_hex().as_bytes());
        out.push(b'\n');
    }
    write_signature(&mut out, b"author", author)?;
    write_signature(&mut out, b"committer", committer)?;
    for (key, value) in extras {
        if key.is_empty()
            || key.iter().any(|byte| matches!(byte, b' ' | b'\n' | 0))
            || RESERVED_HEADERS.contains(&key.as_slice())
        {
            return Err(format!(
                "the extra {:?} cannot be a header",
                String::from_utf8_lossy(key)
            ));
        }
        if value.contains(&0) {
            return Err(format!(
                "the extra {:?} holds a NUL byte",
                String::from_utf8_lossy(key)
            ));
        }
        out.extend_from_slice(key);
        out.push(b' ');
        for (index, line) in value.split(|byte| *byte == b'\n').enumerate() {
            if index > 0 {
                out.extend_from_slice(b"\n ");
            }
            out.extend_from_slice(line);
        }
        out.push(b'\n');
    }
    out.push(b'\n');
    out.extend_from_slice(message);
    Ok(out)
}

fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    haystack.iter().position(|byte| *byte == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> GitOid {
        GitOid::from_bytes([byte; 20])
    }

    #[test]
    fn modes() {
        use EntryType::*;
        let cases: &[(&[u8], Option<EntryType>)] = &[
            (b"100644", Some(File)),
            (b"100664", Some(File)),
            (b"100600", Some(File)),
            (b"0100644", Some(File)),
            (b"100755", Some(Executable)),
            (b"100775", Some(Executable)),
            (b"100700", Some(Executable)),
            (b"120000", Some(Symlink)),
            (b"40000", Some(Tree)),
            (b"040000", Some(Tree)),
            (b"160000", Some(Gitlink)),
            (b"", None),
            (b"100648", None),
            (b"777777777777", None),
            (b"20000", None),
            (b"0", None),
        ];
        for (mode, expected) in cases {
            assert_eq!(
                EntryType::from_mode(mode),
                *expected,
                "{}",
                String::from_utf8_lossy(mode)
            );
        }
        for kind in [File, Executable, Symlink, Tree, Gitlink] {
            assert_eq!(EntryType::from_mode(kind.canonical_mode()), Some(kind));
        }
    }

    #[test]
    fn git_orders_directories_as_if_they_ended_in_a_slash() {
        use Ordering::*;
        assert_eq!(git_order(b"a", false, b"b", false), Less);
        assert_eq!(git_order(b"a.c", false, b"a", true), Less); // "a.c" < "a/"
        assert_eq!(git_order(b"a", true, b"a0", false), Less); // "a/" < "a0"
        assert_eq!(git_order(b"a", false, b"a.c", false), Less);
        assert_eq!(git_order(b"a", false, b"a", true), Less);
        assert_eq!(git_order(b"ab", true, b"a", true), Greater);
        let mut items = [
            TreeItem {
                kind: EntryType::Tree,
                name: b"a",
                oid: oid(1),
            },
            TreeItem {
                kind: EntryType::File,
                name: b"a.c",
                oid: oid(2),
            },
            TreeItem {
                kind: EntryType::File,
                name: b"a0",
                oid: oid(3),
            },
        ];
        let bytes = encode_tree(&mut items);
        let names: Vec<&[u8]> = parse_tree(&bytes).unwrap().iter().map(|e| e.name).collect();
        assert_eq!(names, [&b"a.c"[..], b"a", b"a0"]);
    }

    #[test]
    fn tree_round_trip_and_errors() {
        let mut items = [
            TreeItem {
                kind: EntryType::Executable,
                name: b"run",
                oid: oid(7),
            },
            TreeItem {
                kind: EntryType::Gitlink,
                name: b"sub",
                oid: oid(8),
            },
        ];
        let bytes = encode_tree(&mut items);
        assert!(bytes.starts_with(b"100755 run\0"));
        let entries = parse_tree(&bytes).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].mode, b"160000");
        assert_eq!(entries[1].oid, oid(8));
        assert!(parse_tree(b"").unwrap().is_empty());
        assert!(parse_tree(b"100644").is_err());
        assert!(parse_tree(b"100644 name").is_err());
        assert!(parse_tree(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn commits() {
        let tree = oid(0xaa).to_hex();
        let parent = oid(0xbb).to_hex();
        let text = format!(
            "tree {tree}\nparent {parent}\nparent {}\nauthor A U Thor <a@example.org> 1700000000 +0130\n\
             committer C O Mitter <c@example.org> 1700000001 -0800\nencoding ISO-8859-1\n\
             gpgsig -----BEGIN-----\n \n line\n -----END-----\n\nsubject\n\nbody\n",
            parent.to_uppercase()
        );
        let fields = parse_commit(text.as_bytes()).unwrap();
        assert_eq!(fields.tree, oid(0xaa));
        assert_eq!(fields.parents, [oid(0xbb), oid(0xbb)]);
        assert_eq!(
            parse_signature(fields.author.unwrap()),
            Signature {
                name: b"A U Thor".to_vec(),
                email: b"a@example.org".to_vec(),
                seconds: 1_700_000_000,
                offset_minutes: 90,
            }
        );
        assert_eq!(
            parse_signature(fields.committer.unwrap()).offset_minutes,
            -480
        );
        assert_eq!(fields.message, b"subject\n\nbody\n");

        // No message at all, and an empty one.
        let bare = format!("tree {tree}\nauthor x <y> 1 +0000\n");
        assert_eq!(parse_commit(bare.as_bytes()).unwrap().message, b"");
        let empty = format!("tree {tree}\n\n");
        let fields = parse_commit(empty.as_bytes()).unwrap();
        assert_eq!(fields.message, b"");
        assert_eq!(fields.author, None);

        // A parent after another header is not a parent.
        let late = format!("tree {tree}\nauthor x <y> 1 +0000\nparent {parent}\n\n");
        assert!(parse_commit(late.as_bytes()).unwrap().parents.is_empty());

        assert!(parse_commit(b"").is_err());
        assert!(parse_commit(b"parent x\n").is_err());
        assert!(parse_commit(b"tree 1234\n").is_err());
    }

    /// Name, email, seconds and offset.
    type Expected<'a> = (&'a [u8], &'a [u8], i64, i64);

    #[test]
    fn odd_signatures_never_fail() {
        let cases: &[(&[u8], Expected<'_>)] = &[
            (b"Name <e> 5 +0000", (b"Name", b"e", 5, 0)),
            (b"Name<e>5 -0130", (b"Name", b"e", 5, -90)),
            (b"  Spaced   <e> 7 +0200  ", (b"Spaced", b"e", 7, 120)),
            (b"<> 0 +0000", (b"", b"", 0, 0)),
            (b"No email at all", (b"No email at all", b"", 0, 0)),
            (b"Open <bracket only", (b"Open <bracket only", b"", 0, 0)),
            (b"Name <e>", (b"Name", b"e", 0, 0)),
            (b"Name <e> garbage +0100", (b"Name", b"e", 0, 60)),
            (b"Name <e> -5 +0000", (b"Name", b"e", -5, 0)),
            (
                b"Name <e> 99999999999999999999 +0000",
                (b"Name", b"e", 0, 0),
            ),
            (b"Name <e> 1 +05030", (b"Name", b"e", 1, 3030)),
            // The email ends at the first '>'; the next words are read as
            // they come, so "b>" is an unreadable time and "1" the offset.
            (b"Name <a> b> 1 +0000", (b"Name", b"a", 0, 1)),
        ];
        for (line, (name, email, seconds, offset)) in cases {
            let signature = parse_signature(line);
            assert_eq!(
                (
                    signature.name.as_slice(),
                    signature.email.as_slice(),
                    signature.seconds,
                    signature.offset_minutes
                ),
                (*name, *email, *seconds, *offset),
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn generated_commits_read_back() {
        let author = Signature {
            name: b"Ada Lovelace".to_vec(),
            email: b"ada@example.org".to_vec(),
            seconds: 1_700_000_000,
            offset_minutes: -330,
        };
        let extras = vec![(b"x-note".to_vec(), b"one\ntwo".to_vec())];
        let bytes = encode_commit(oid(1), &[oid(2)], &author, &author, &extras, b"msg\n").unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains("author Ada Lovelace <ada@example.org> 1700000000 -0530\n"));
        assert!(text.contains("x-note one\n two\n\nmsg\n"));
        let fields = parse_commit(&bytes).unwrap();
        assert_eq!(fields.tree, oid(1));
        assert_eq!(fields.parents, [oid(2)]);
        assert_eq!(parse_signature(fields.author.unwrap()), author);
        assert_eq!(fields.message, b"msg\n");

        let mut bad = author.clone();
        bad.name = b"a <b>".to_vec();
        assert!(encode_commit(oid(1), &[], &bad, &author, &[], b"").is_err());
        let mut far = author.clone();
        far.offset_minutes = 100 * 60;
        assert!(encode_commit(oid(1), &[], &far, &author, &[], b"").is_err());
        for key in [&b""[..], b"tree", b"a b", b"a\nb"] {
            let extras = vec![(key.to_vec(), b"v".to_vec())];
            assert!(encode_commit(oid(1), &[], &author, &author, &extras, b"").is_err());
        }
    }

    #[test]
    fn tags() {
        let target = oid(0x42);
        let text = format!(
            "object {target}\ntype commit\ntag v1\ntagger T <t@example.org> 1 +0000\n\nmsg\n"
        );
        assert_eq!(parse_tag(text.as_bytes()), Ok(target));
        assert!(parse_tag(b"type commit\n").is_err());
    }
}
