// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Three-way merges of file contents, line by line.
//!
//! Both sides are compared with the base, using imara-diff's histogram diff.
//! Each change is a hunk: a range of base lines replaced by a range of lines
//! of that side. Hunks of the two sides that overlap or touch form one region:
//! where only one side changed, its lines are taken; where both changed the
//! region to the same lines, those are taken; otherwise the region is a
//! conflict. Changes on adjacent lines therefore conflict, as they do in git.

use core::ops::Range;

use imara_diff::sources::byte_lines;
use imara_diff::{Algorithm, Diff, Hunk, IndentHeuristic, IndentLevel, Interner, Token};

/// Content with a NUL byte among this many leading bytes is binary, the rule
/// git uses.
pub const BINARY_SNIFF_BYTES: usize = 8000;

/// Content this long or longer is not merged line by line.
const MAX_TEXT_BYTES: usize = i32::MAX as usize;

/// Tab width for the indentation heuristic that places ambiguous hunks.
const TAB_WIDTH: u8 = 8;

/// Whether content is binary: it has a NUL byte among its first
/// [`BINARY_SNIFF_BYTES`] bytes.
pub fn is_binary(content: &[u8]) -> bool {
    content[..content.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

/// The outcome of a three-way merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeResult {
    /// The merged content.
    Clean(Vec<u8>),
    /// The sides changed the same lines differently. A conflict on binary
    /// content has no hunks: it cannot be split into lines.
    Conflict {
        /// Each conflicting region, in order.
        hunks: Vec<ConflictHunk>,
    },
}

impl MergeResult {
    /// Whether the merge succeeded.
    pub fn is_clean(&self) -> bool {
        matches!(self, MergeResult::Clean(_))
    }
}

/// One region that the two sides changed differently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictHunk {
    /// The region in the base.
    pub base: HunkSide,
    /// What ours has instead.
    pub ours: HunkSide,
    /// What theirs has instead.
    pub theirs: HunkSide,
}

/// A run of lines of one input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkSide {
    /// Line numbers, counted from 0, end excluded. An empty range sits before
    /// the line it starts at.
    pub lines: Range<usize>,
    /// The lines themselves, with their line endings.
    pub text: Vec<u8>,
}

/// Merges `ours` and `theirs`, two versions of `base`.
///
/// Lines end after each `\n`; a last line without one is a line of its own,
/// so adding or removing a final newline is a change to that line. Changes
/// that overlap, touch (on adjacent lines, or an insertion right next to a
/// change) or insert at the same place conflict, unless both sides made them
/// identically.
///
/// If one side equals the base, or both sides are equal, the result is the
/// other side, whatever the content. Otherwise binary content (see
/// [`is_binary`]) is a conflict without hunks.
pub fn merge3(base: &[u8], ours: &[u8], theirs: &[u8]) -> MergeResult {
    if ours == theirs || theirs == base {
        return MergeResult::Clean(ours.to_vec());
    }
    if ours == base {
        return MergeResult::Clean(theirs.to_vec());
    }
    if [base, ours, theirs]
        .iter()
        .any(|content| content.len() >= MAX_TEXT_BYTES || is_binary(content))
    {
        return MergeResult::Conflict { hunks: Vec::new() };
    }
    Merge::new(base, ours, theirs).run()
}

/// The lines of one input and their interned tokens.
struct Text<'a> {
    lines: Vec<&'a [u8]>,
    tokens: Vec<Token>,
}

impl<'a> Text<'a> {
    fn new(content: &'a [u8], interner: &mut Interner<&'a [u8]>) -> Self {
        let lines: Vec<&[u8]> = byte_lines(content).collect();
        let tokens = lines.iter().map(|&line| interner.intern(line)).collect();
        Text { lines, tokens }
    }

    fn side(&self, lines: Range<usize>) -> HunkSide {
        HunkSide {
            text: self.lines[lines.clone()].concat(),
            lines,
        }
    }
}

struct Merge<'a> {
    base: Text<'a>,
    ours: Text<'a>,
    theirs: Text<'a>,
    interner: Interner<&'a [u8]>,
}

impl<'a> Merge<'a> {
    fn new(base: &'a [u8], ours: &'a [u8], theirs: &'a [u8]) -> Self {
        let mut interner = Interner::new(0);
        let base = Text::new(base, &mut interner);
        let ours = Text::new(ours, &mut interner);
        let theirs = Text::new(theirs, &mut interner);
        Merge {
            base,
            ours,
            theirs,
            interner,
        }
    }

    /// The hunks that turn the base into `side`.
    fn hunks(&self, side: &Text<'_>) -> Vec<Hunk> {
        let mut diff = Diff::default();
        diff.compute_with(
            Algorithm::Histogram,
            &self.base.tokens,
            &side.tokens,
            self.interner.num_tokens(),
        );
        diff.postprocess_with(
            &self.base.tokens,
            &side.tokens,
            IndentHeuristic::new(|token| {
                IndentLevel::for_ascii_line(self.interner[token].iter().copied(), TAB_WIDTH)
            }),
        );
        diff.hunks().collect()
    }

    fn run(self) -> MergeResult {
        let ours = self.hunks(&self.ours);
        let theirs = self.hunks(&self.theirs);
        let mut out: Vec<u8> = Vec::new();
        let mut hunks = Vec::new();
        let (mut i, mut j) = (0, 0);
        // Base lines before `copied` are in the output already.
        let mut copied = 0;
        // Line number on each side minus line number in the base, for the
        // lines between regions.
        let (mut ours_shift, mut theirs_shift) = (0isize, 0isize);

        while i < ours.len() || j < theirs.len() {
            // A region starts with the hunk that starts first, and grows while
            // a hunk of either side starts inside it or right at its end.
            let ours_first = match (ours.get(i), theirs.get(j)) {
                (Some(o), Some(t)) => o.before.start <= t.before.start,
                (o, _) => o.is_some(),
            };
            let first = if ours_first { &ours[i] } else { &theirs[j] };
            let start = first.before.start as usize;
            let mut end = first.before.end as usize;
            let (i0, j0) = (i, j);
            if ours_first {
                i += 1;
            } else {
                j += 1;
            }
            loop {
                if let Some(hunk) = ours.get(i).filter(|h| h.before.start as usize <= end) {
                    end = end.max(hunk.before.end as usize);
                    i += 1;
                } else if let Some(hunk) = theirs.get(j).filter(|h| h.before.start as usize <= end)
                {
                    end = end.max(hunk.before.end as usize);
                    j += 1;
                } else {
                    break;
                }
            }

            let ours_growth = growth(&ours[i0..i]);
            let theirs_growth = growth(&theirs[j0..j]);
            let ours_lines = shift(start, ours_shift)..shift(end, ours_shift + ours_growth);
            let theirs_lines = shift(start, theirs_shift)..shift(end, theirs_shift + theirs_growth);
            ours_shift += ours_growth;
            theirs_shift += theirs_growth;

            push_lines(&mut out, &self.base.lines[copied..start]);
            copied = end;
            if j == j0 {
                push_lines(&mut out, &self.ours.lines[ours_lines]);
            } else if i == i0 {
                push_lines(&mut out, &self.theirs.lines[theirs_lines]);
            } else if self.ours.tokens[ours_lines.clone()]
                == self.theirs.tokens[theirs_lines.clone()]
            {
                // Both sides made the same change.
                push_lines(&mut out, &self.ours.lines[ours_lines]);
            } else {
                hunks.push(ConflictHunk {
                    base: self.base.side(start..end),
                    ours: self.ours.side(ours_lines),
                    theirs: self.theirs.side(theirs_lines),
                });
            }
        }
        push_lines(&mut out, &self.base.lines[copied..]);

        if hunks.is_empty() {
            MergeResult::Clean(out)
        } else {
            MergeResult::Conflict { hunks }
        }
    }
}

fn push_lines(out: &mut Vec<u8>, lines: &[&[u8]]) {
    for line in lines {
        out.extend_from_slice(line);
    }
}

/// How many lines the hunks add, minus how many they remove.
fn growth(hunks: &[Hunk]) -> isize {
    hunks
        .iter()
        .map(|hunk| hunk.after.len() as isize - hunk.before.len() as isize)
        .sum()
}

fn shift(line: usize, by: isize) -> usize {
    line.checked_add_signed(by)
        .expect("a hunk never maps a line before the start")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(base: &str, ours: &str, theirs: &str) -> String {
        match merge3(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()) {
            MergeResult::Clean(bytes) => String::from_utf8(bytes).unwrap(),
            conflict => panic!("expected a clean merge, got {conflict:?}"),
        }
    }

    fn conflict(base: &str, ours: &str, theirs: &str) -> Vec<ConflictHunk> {
        match merge3(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()) {
            MergeResult::Conflict { hunks } => hunks,
            clean => panic!("expected a conflict, got {clean:?}"),
        }
    }

    fn side(lines: Range<usize>, text: &str) -> HunkSide {
        HunkSide {
            lines,
            text: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn one_side_unchanged() {
        assert_eq!(clean("a\nb\n", "a\nb\n", "a\nc\n"), "a\nc\n");
        assert_eq!(clean("a\nb\n", "a\nc\n", "a\nb\n"), "a\nc\n");
        assert_eq!(clean("a\n", "x\n", "x\n"), "x\n");
    }

    #[test]
    fn separate_edits_both_apply() {
        let base = "1\n2\n3\n4\n5\n6\n7\n";
        let ours = "1\nTWO\n3\n4\n5\n6\n7\n";
        let theirs = "1\n2\n3\n4\n5\nSIX\n7\n";
        assert_eq!(clean(base, ours, theirs), "1\nTWO\n3\n4\n5\nSIX\n7\n");
        assert_eq!(clean(base, theirs, ours), "1\nTWO\n3\n4\n5\nSIX\n7\n");
        // Insertions and deletions far apart.
        let ours = "0\n1\n2\n3\n4\n5\n6\n7\n";
        let theirs = "1\n2\n3\n4\n5\n6\n";
        assert_eq!(clean(base, ours, theirs), "0\n1\n2\n3\n4\n5\n6\n");
        // One unchanged line between the edits is enough.
        let ours = "1\nTWO\n3\n4\n5\n6\n7\n";
        let theirs = "1\n2\n3\nFOUR\n5\n6\n7\n";
        assert_eq!(clean(base, ours, theirs), "1\nTWO\n3\nFOUR\n5\n6\n7\n");
    }

    #[test]
    fn overlapping_edits_conflict() {
        let hunks = conflict("1\n2\n3\n", "1\nX\n3\n", "1\nY\n3\n");
        assert_eq!(
            hunks,
            [ConflictHunk {
                base: side(1..2, "2\n"),
                ours: side(1..2, "X\n"),
                theirs: side(1..2, "Y\n"),
            }]
        );
        // Partly overlapping ranges make one region.
        let hunks = conflict("1\n2\n3\n4\n5\n", "1\nA\nB\n4\n5\n", "1\n2\nC\nD\n5\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].base, side(1..4, "2\n3\n4\n"));
        assert_eq!(hunks[0].ours, side(1..4, "A\nB\n4\n"));
        assert_eq!(hunks[0].theirs, side(1..4, "2\nC\nD\n"));
    }

    #[test]
    fn adjacent_edits_conflict() {
        let hunks = conflict("1\n2\n3\n4\n", "1\nX\n3\n4\n", "1\n2\nY\n4\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].base, side(1..3, "2\n3\n"));
        assert_eq!(hunks[0].ours, side(1..3, "X\n3\n"));
        assert_eq!(hunks[0].theirs, side(1..3, "2\nY\n"));
        // An insertion right after a changed line touches it.
        conflict("1\n2\n3\n", "1\nX\n3\n", "1\n2\nnew\n3\n");
    }

    #[test]
    fn identical_edits_are_clean() {
        let base = "1\n2\n3\n4\n5\n";
        let both = "1\nX\n3\n4\nY\n";
        assert_eq!(clean(base, both, both), both);
        // The same edit on both sides, plus a separate one on one side.
        let ours = "1\nX\n3\n4\nZ\n";
        let theirs = "1\nX\n3\n4\n5\n";
        assert_eq!(clean(base, ours, theirs), ours);
        // Both delete the same line.
        assert_eq!(clean(base, "1\n3\n4\n5\n", "1\n3\n4\n5\n"), "1\n3\n4\n5\n");
    }

    #[test]
    fn insertions_at_the_same_place() {
        let hunks = conflict("a\nb\n", "a\nX\nb\n", "a\nY\nb\n");
        assert_eq!(
            hunks,
            [ConflictHunk {
                base: side(1..1, ""),
                ours: side(1..2, "X\n"),
                theirs: side(1..2, "Y\n"),
            }]
        );
        // The same insertion is clean, and kept once.
        assert_eq!(clean("a\nb\n", "a\nX\nb\n", "a\nX\nb\n"), "a\nX\nb\n");
        // At the very start and the very end.
        conflict("a\n", "X\na\n", "Y\na\n");
        conflict("a\n", "a\nX\n", "a\nY\n");
        assert_eq!(
            clean("a\nb\nc\n", "X\na\nb\nc\n", "a\nb\nc\nY\n"),
            "X\na\nb\nc\nY\n"
        );
    }

    #[test]
    fn empty_files() {
        assert_eq!(clean("", "", "new\n"), "new\n");
        assert_eq!(clean("", "", ""), "");
        assert_eq!(clean("a\n", "", "a\n"), "");
        // Both sides add different content to an empty file.
        let hunks = conflict("", "x\n", "y\n");
        assert_eq!(hunks[0].base, side(0..0, ""));
        // One side empties the file, the other edits it.
        conflict("a\nb\n", "", "a\nc\n");
    }

    #[test]
    fn missing_final_newline() {
        // Adding a newline at the end changes the last line.
        assert_eq!(clean("a\nb", "a\nb\n", "a\nb"), "a\nb\n");
        // An edit at the start and a new last line at the end.
        assert_eq!(clean("a\nb\nc", "A\nb\nc", "a\nb\nc\nd"), "A\nb\nc\nd");
        // Both change the unterminated last line.
        let hunks = conflict("a\nb", "a\nc", "a\nd");
        assert_eq!(hunks[0].base, side(1..2, "b"));
        assert_eq!(hunks[0].ours, side(1..2, "c"));
        assert_eq!(hunks[0].theirs, side(1..2, "d"));
        // One adds the newline, the other changes the line.
        conflict("a\nb", "a\nb\n", "a\nB");
    }

    #[test]
    fn binary_content() {
        assert!(is_binary(b"a\0b"));
        assert!(!is_binary(b"plain text\n"));
        let mut late = vec![b'a'; BINARY_SNIFF_BYTES];
        late.push(0);
        assert!(!is_binary(&late), "only the first bytes are looked at");

        let base = b"\0base";
        assert_eq!(
            merge3(base, base, b"\0theirs"),
            MergeResult::Clean(b"\0theirs".to_vec())
        );
        assert_eq!(
            merge3(base, b"\0ours", base),
            MergeResult::Clean(b"\0ours".to_vec())
        );
        assert_eq!(
            merge3(base, b"\0same", b"\0same"),
            MergeResult::Clean(b"\0same".to_vec())
        );
        assert_eq!(
            merge3(base, b"\0ours", b"\0theirs"),
            MergeResult::Conflict { hunks: Vec::new() }
        );
        // Text on two sides is not enough when the base is binary.
        assert_eq!(
            merge3(b"a\0\n1\n2\n3\n", b"a\0\nX\n2\n3\n", b"a\0\n1\n2\nY\n"),
            MergeResult::Conflict { hunks: Vec::new() }
        );
    }

    #[test]
    fn several_conflicts_and_clean_parts_between_them() {
        let base = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let ours = "1\nA\n3\n4\nX\n6\n7\nC\n9\n";
        let theirs = "1\nB\n3\n4\n5\n6\n7\nD\n9\n";
        let hunks = conflict(base, ours, theirs);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].ours, side(1..2, "A\n"));
        assert_eq!(hunks[1].base, side(7..8, "8\n"));
        assert_eq!(hunks[1].theirs, side(7..8, "D\n"));
    }

    #[test]
    fn line_numbers_follow_earlier_growth() {
        // Ours grows the file before the conflict; theirs shrinks it.
        let base = "1\n2\n3\n4\n5\n6\n";
        let ours = "1\n2\nnew\nnew2\n3\n4\nX\n6\n";
        let theirs = "2\n3\n4\nY\n6\n";
        let hunks = conflict(base, ours, theirs);
        assert_eq!(
            hunks,
            [ConflictHunk {
                base: side(4..5, "5\n"),
                ours: side(6..7, "X\n"),
                theirs: side(3..4, "Y\n"),
            }]
        );
        // An insertion right after a line the other side deletes touches it.
        conflict(base, "1\nnew\n2\n3\n4\n5\n6\n", "2\n3\n4\n5\n6\n");
    }
}
