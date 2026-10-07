//! Turns (baseline, previous, current) contents into a renderable diff.
//!
//! Besides the usual baseline→current diff, we also diff previous→current so
//! each line can be flagged `fresh` when it changed in the latest edit. That's
//! what lets a frontend highlight and scroll to "what the agent just did".

use std::collections::HashSet;
use std::time::{Duration, Instant};

use similar::{Algorithm, DiffOp, DiffTag, capture_diff_slices_deadline, group_diff_ops};

use crate::baseline::Content;

const CONTEXT_LINES: usize = 3;
const DIFF_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    HunkHeader,
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: LineKind,
    /// 1-based line numbers.
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    /// Line text without its line terminator.
    pub text: String,
    /// Changed by the most recent edit.
    pub fresh: bool,
}

#[derive(Clone, Debug)]
pub enum Body {
    Text(Vec<DiffLine>),
    Binary,
    TooLarge,
}

#[derive(Clone, Debug)]
pub struct FileDiff {
    /// Path relative to the repository root, `/`-separated.
    pub path: String,
    pub status: FileStatus,
    pub added: usize,
    pub removed: usize,
    pub body: Body,
    /// Index into the body's lines of the most interesting line to show:
    /// the first fresh line, else the first changed line.
    pub focus: Option<usize>,
    /// Monotonic sequence number of the change that produced this diff.
    pub seq: u64,
    pub at: Instant,
}

impl FileDiff {
    pub fn lines(&self) -> &[DiffLine] {
        match &self.body {
            Body::Text(lines) => lines,
            _ => &[],
        }
    }
}

/// Computes the diff from `base` to `current`. `prev` is what we last saw for
/// this file (`None` means "treat everything as fresh"). Returns `None` when
/// the file matches its baseline.
pub fn compute(
    path: &str,
    base: &Content,
    prev: Option<&Content>,
    current: &Content,
    seq: u64,
) -> Option<FileDiff> {
    let status = match (base, current) {
        (Content::Absent, Content::Absent) => return None,
        (a, b) if a == b => return None,
        (Content::Absent, _) => FileStatus::Added,
        (_, Content::Absent) => FileStatus::Deleted,
        _ => FileStatus::Modified,
    };
    let mut diff = FileDiff {
        path: path.to_string(),
        status,
        added: 0,
        removed: 0,
        body: Body::TooLarge,
        focus: None,
        seq,
        at: Instant::now(),
    };

    let (Some(old), Some(new)) = (bytes(base), bytes(current)) else {
        return Some(diff); // one side too large
    };
    if is_binary(old) || is_binary(new) {
        diff.body = Body::Binary;
        return Some(diff);
    }

    let old_text = String::from_utf8_lossy(old);
    let new_text = String::from_utf8_lossy(new);
    let old_lines = split_lines(&old_text);
    let new_lines = split_lines(&new_text);
    let ops = line_ops(&old_lines, &new_lines);

    // Freshness: which current lines were just inserted, and which baseline
    // lines were still present in the previous version (so removing them now
    // is a fresh removal).
    let prev_text = prev.and_then(bytes).map(String::from_utf8_lossy);
    let (fresh_new, fresh_old) = match &prev_text {
        None => (None, None),
        Some(prev_text) => {
            let prev_lines = split_lines(prev_text);
            let mut inserted = HashSet::new();
            for op in line_ops(&prev_lines, &new_lines) {
                if matches!(op.tag(), DiffTag::Insert | DiffTag::Replace) {
                    inserted.extend(op.new_range());
                }
            }
            let mut still_present = HashSet::new();
            for op in line_ops(&old_lines, &prev_lines) {
                if op.tag() == DiffTag::Equal {
                    still_present.extend(op.old_range());
                }
            }
            (Some(inserted), Some(still_present))
        }
    };
    let is_fresh_new = |j: usize| fresh_new.as_ref().is_none_or(|s| s.contains(&j));
    let is_fresh_old = |i: usize| fresh_old.as_ref().is_none_or(|s| s.contains(&i));

    let mut lines = Vec::new();
    for group in group_diff_ops(ops, CONTEXT_LINES) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let (o1, o2) = (first.old_range().start, last.old_range().end);
        let (n1, n2) = (first.new_range().start, last.new_range().end);
        lines.push(DiffLine {
            kind: LineKind::HunkHeader,
            old_no: None,
            new_no: None,
            text: format!(
                "@@ -{},{} +{},{} @@",
                hunk_start(o1, o2),
                o2 - o1,
                hunk_start(n1, n2),
                n2 - n1
            ),
            fresh: false,
        });
        for op in &group {
            let (tag, old_range, new_range) = op.as_tag_tuple();
            match tag {
                DiffTag::Equal => {
                    for (i, j) in old_range.zip(new_range) {
                        lines.push(DiffLine {
                            kind: LineKind::Context,
                            old_no: Some(i + 1),
                            new_no: Some(j + 1),
                            text: trim_eol(new_lines[j]).to_string(),
                            fresh: false,
                        });
                    }
                }
                DiffTag::Delete | DiffTag::Insert | DiffTag::Replace => {
                    for i in old_range {
                        diff.removed += 1;
                        lines.push(DiffLine {
                            kind: LineKind::Removed,
                            old_no: Some(i + 1),
                            new_no: None,
                            text: trim_eol(old_lines[i]).to_string(),
                            fresh: is_fresh_old(i),
                        });
                    }
                    for j in new_range {
                        diff.added += 1;
                        lines.push(DiffLine {
                            kind: LineKind::Added,
                            old_no: None,
                            new_no: Some(j + 1),
                            text: trim_eol(new_lines[j]).to_string(),
                            fresh: is_fresh_new(j),
                        });
                    }
                }
            }
        }
    }

    diff.focus = lines
        .iter()
        .position(|l| l.fresh)
        .or_else(|| lines.iter().position(|l| l.kind != LineKind::HunkHeader && l.kind != LineKind::Context));
    diff.body = Body::Text(lines);
    Some(diff)
}

fn bytes(c: &Content) -> Option<&[u8]> {
    match c {
        Content::Absent => Some(&[]),
        Content::Bytes(b) => Some(b),
        Content::TooLarge(_) => None,
    }
}

fn is_binary(b: &[u8]) -> bool {
    b[..b.len().min(8000)].contains(&0)
}

/// Splits into lines, keeping terminators so a missing final newline is a
/// visible change.
fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn trim_eol(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

fn line_ops(old: &[&str], new: &[&str]) -> Vec<DiffOp> {
    let deadline = Instant::now() + DIFF_TIMEOUT;
    capture_diff_slices_deadline(Algorithm::Myers, old, new, Some(deadline))
}

/// Unified-diff convention: an empty range is reported at the line before it.
fn hunk_start(start: usize, end: usize) -> usize {
    if start == end { start } else { start + 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Content {
        Content::Bytes(s.as_bytes().into())
    }

    fn kinds(d: &FileDiff) -> Vec<(LineKind, &str, bool)> {
        d.lines()
            .iter()
            .filter(|l| l.kind != LineKind::HunkHeader)
            .map(|l| (l.kind, l.text.as_str(), l.fresh))
            .collect()
    }

    #[test]
    fn identical_is_none() {
        assert!(compute("f", &text("a\n"), None, &text("a\n"), 0).is_none());
        assert!(compute("f", &Content::Absent, None, &Content::Absent, 0).is_none());
    }

    #[test]
    fn status_and_counts() {
        let d = compute("f", &Content::Absent, None, &text("a\nb\n"), 0).unwrap();
        assert_eq!(d.status, FileStatus::Added);
        assert_eq!((d.added, d.removed), (2, 0));

        let d = compute("f", &text("a\n"), None, &Content::Absent, 0).unwrap();
        assert_eq!(d.status, FileStatus::Deleted);
        assert_eq!((d.added, d.removed), (0, 1));

        let d = compute("f", &text("a\nb\n"), None, &text("a\nc\n"), 0).unwrap();
        assert_eq!(d.status, FileStatus::Modified);
        assert_eq!((d.added, d.removed), (1, 1));
        assert_eq!(d.lines()[0].text, "@@ -1,2 +1,2 @@");
    }

    #[test]
    fn freshness_tracks_latest_edit_only() {
        let base = text("a\nb\nc\n");
        let prev = text("a\nX\nb\nc\n"); // earlier edit inserted X
        let cur = text("a\nX\nb\nY\n"); // latest edit replaced c with Y
        let d = compute("f", &base, Some(&prev), &cur, 0).unwrap();
        assert_eq!(
            kinds(&d),
            vec![
                (LineKind::Context, "a", false),
                (LineKind::Added, "X", false),
                (LineKind::Context, "b", false),
                (LineKind::Removed, "c", true),
                (LineKind::Added, "Y", true),
            ]
        );
        // Focus lands on the first fresh line (index includes the hunk header).
        assert_eq!(d.lines()[d.focus.unwrap()].text, "c");
    }

    #[test]
    fn binary_and_large() {
        let d = compute("f", &text("a"), None, &Content::Bytes(vec![0, 1].into()), 0).unwrap();
        assert!(matches!(d.body, Body::Binary));
        let d = compute("f", &text("a"), None, &Content::TooLarge(1 << 30), 0).unwrap();
        assert!(matches!(d.body, Body::TooLarge));
    }
}
