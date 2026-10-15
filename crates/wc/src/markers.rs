// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Conflict markers, in the style git writes them:
//!
//! ```text
//! <<<<<<< ours label
//! the lines of ours
//! =======
//! the lines of theirs
//! >>>>>>> theirs label
//! ```
//!
//! The regions come from the engine's three-way merge, so they are exactly
//! the hunks a conflict reports. The parts between them are merged again on
//! their own, which gives the clean result the whole merge would have had.

use braid_engine::{ConflictHunk, MergeResult, merge3};

/// The first line of a conflict region starts with this.
const OURS: &[u8] = b"<<<<<<<";
/// The line between the two sides.
const SEPARATOR: &[u8] = b"=======";
/// The last line of a conflict region starts with this.
const THEIRS: &[u8] = b">>>>>>>";

/// `ours` and `theirs` merged against `base`, with each conflicting region
/// written between markers, or `None` when the content cannot be merged line
/// by line (binary content) or merges cleanly.
pub(crate) fn render(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    ours_label: &str,
    theirs_label: &str,
) -> Option<Vec<u8>> {
    let MergeResult::Conflict { hunks } = merge3(base, ours, theirs) else {
        return None;
    };
    if hunks.is_empty() {
        return None;
    }
    let base_lines = lines(base);
    let ours_lines = lines(ours);
    let theirs_lines = lines(theirs);
    let mut out = Vec::with_capacity(ours.len() + theirs.len());
    let (mut b, mut o, mut t) = (0, 0, 0);
    for hunk in &hunks {
        let ConflictHunk {
            base: hb,
            ours: ho,
            theirs: ht,
        } = hunk;
        merge_between(
            &mut out,
            &base_lines[b..hb.lines.start],
            &ours_lines[o..ho.lines.start],
            &theirs_lines[t..ht.lines.start],
            ours_label,
            theirs_label,
        );
        region(&mut out, &ho.text, &ht.text, ours_label, theirs_label);
        (b, o, t) = (hb.lines.end, ho.lines.end, ht.lines.end);
    }
    merge_between(
        &mut out,
        &base_lines[b..],
        &ours_lines[o..],
        &theirs_lines[t..],
        ours_label,
        theirs_label,
    );
    Some(out)
}

/// Whether `content` still holds a line that starts a conflict region,
/// separates its sides or ends it.
pub(crate) fn has_markers(content: &[u8]) -> bool {
    lines(content).iter().any(|line| {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        line == SEPARATOR
            || [OURS, THEIRS].iter().any(|marker| {
                line.starts_with(marker)
                    && (line.len() == marker.len() || line[marker.len()] == b' ')
            })
    })
}

/// Lines, each with its `\n`; a last line without one is a line too.
fn lines(content: &[u8]) -> Vec<&[u8]> {
    content.split_inclusive(|byte| *byte == b'\n').collect()
}

/// The lines between two conflict regions, which merged cleanly as part of
/// the whole; merged again on their own. Should that disagree, the stretch
/// is shown as a conflict too, rather than guessed.
fn merge_between(
    out: &mut Vec<u8>,
    base: &[&[u8]],
    ours: &[&[u8]],
    theirs: &[&[u8]],
    ours_label: &str,
    theirs_label: &str,
) {
    let (base, ours, theirs) = (base.concat(), ours.concat(), theirs.concat());
    match merge3(&base, &ours, &theirs) {
        MergeResult::Clean(merged) => out.extend_from_slice(&merged),
        MergeResult::Conflict { .. } => region(out, &ours, &theirs, ours_label, theirs_label),
    }
}

fn region(out: &mut Vec<u8>, ours: &[u8], theirs: &[u8], ours_label: &str, theirs_label: &str) {
    marker_line(out, OURS, ours_label);
    side(out, ours);
    out.extend_from_slice(SEPARATOR);
    out.push(b'\n');
    side(out, theirs);
    marker_line(out, THEIRS, theirs_label);
}

fn marker_line(out: &mut Vec<u8>, marker: &[u8], label: &str) {
    out.extend_from_slice(marker);
    if !label.is_empty() {
        out.push(b' ');
        out.extend_from_slice(label.replace('\n', " ").as_bytes());
    }
    out.push(b'\n');
}

/// One side of a region, ending with a newline so the next marker starts a
/// line.
fn side(out: &mut Vec<u8>, text: &[u8]) {
    out.extend_from_slice(text);
    if !text.is_empty() && !text.ends_with(b"\n") {
        out.push(b'\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: Option<Vec<u8>>) -> String {
        String::from_utf8(bytes.expect("a conflict")).unwrap()
    }

    #[test]
    fn one_region_between_clean_parts() {
        let base = "1\n2\n3\n4\n5\n6\n7\n";
        let ours = "ONE\n2\n3\nours\n5\n6\n7\n";
        let theirs = "1\n2\n3\ntheirs\n5\n6\nSEVEN\n";
        let merged = text(render(
            base.as_bytes(),
            ours.as_bytes(),
            theirs.as_bytes(),
            "main",
            "mine",
        ));
        assert_eq!(
            merged,
            "ONE\n2\n3\n<<<<<<< main\nours\n=======\ntheirs\n>>>>>>> mine\n5\n6\nSEVEN\n"
        );
        assert!(has_markers(merged.as_bytes()));
    }

    #[test]
    fn several_regions_and_a_last_line_without_newline() {
        let base = "a\nb\nc\nd\ne\nf";
        let ours = "A\nb\nc\nd\ne\nF";
        let theirs = "a2\nb\nc\nd\ne\nf2";
        let merged = text(render(
            base.as_bytes(),
            ours.as_bytes(),
            theirs.as_bytes(),
            "o",
            "t",
        ));
        assert_eq!(
            merged,
            "<<<<<<< o\nA\n=======\na2\n>>>>>>> t\nb\nc\nd\ne\n<<<<<<< o\nF\n=======\nf2\n>>>>>>> t\n"
        );
    }

    #[test]
    fn nothing_to_mark() {
        assert_eq!(render(b"a\n", b"b\n", b"a\n", "o", "t"), None);
        assert_eq!(render(b"\0a", b"\0b", b"\0c", "o", "t"), None);
    }

    #[test]
    fn marker_detection() {
        assert!(!has_markers(b"plain\ntext\n"));
        assert!(!has_markers(b"<<<<<<<< eight\n========\n"));
        assert!(!has_markers(b"a <<<<<<< b\n"));
        assert!(has_markers(b"x\n=======\r\ny\n"));
        assert!(has_markers(b">>>>>>> theirs"));
        assert!(has_markers(b"<<<<<<<\n"));
    }
}
