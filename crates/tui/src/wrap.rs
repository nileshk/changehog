//! Splitting diff lines into screen rows for soft wrap.
//!
//! Lines are broken at the column limit rather than at word boundaries,
//! which suits code. Widths are display widths, so wide characters (CJK,
//! emoji) take two columns.

use changehog_core::{DiffLine, FileDiff, LineKind};
use unicode_width::UnicodeWidthChar;

/// Tabs are shown as this many spaces.
const TAB: &str = "    ";

/// Line text as displayed, with tabs expanded.
pub fn display_text(line: &DiffLine) -> String {
    line.text.replace('\t', TAB)
}

/// Width of the line-number columns for `diff`.
pub fn num_width(diff: &FileDiff) -> usize {
    diff.lines()
        .iter()
        .filter_map(|l| l.old_no.max(l.new_no))
        .max()
        .unwrap_or(1)
        .to_string()
        .len()
        .max(3)
}

/// Columns before a line's text: the fresh marker, both line numbers with
/// a space after each, and the sign with a space.
fn gutter_width(num_width: usize) -> usize {
    1 + (num_width + 1) * 2 + 2
}

/// Columns available for a line's text in an area `area_width` wide.
pub fn text_width(kind: LineKind, area_width: usize, num_width: usize) -> usize {
    let gutter = match kind {
        LineKind::HunkHeader => 1,
        _ => gutter_width(num_width),
    };
    area_width.saturating_sub(gutter).max(1)
}

/// Splits `text` into pieces at most `width` columns wide. There's always at
/// least one piece; a character wider than `width` gets a piece of its own.
pub fn chunks(text: &str, width: usize) -> Vec<&str> {
    let width = width.max(1);
    let mut out = Vec::new();
    let (mut start, mut col) = (0, 0);
    for (i, c) in text.char_indices() {
        let w = c.width().unwrap_or(0);
        if col + w > width && col > 0 {
            out.push(&text[start..i]);
            start = i;
            col = 0;
        }
        col += w;
    }
    out.push(&text[start..]);
    out
}

/// Rows each of `diff`'s lines takes when wrapped to `area_width` columns.
pub fn line_rows(diff: &FileDiff, area_width: usize) -> Vec<usize> {
    let num_width = num_width(diff);
    diff.lines()
        .iter()
        .map(|l| chunks(&display_text(l), text_width(l.kind, area_width, num_width)).len())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_the_column_limit() {
        assert_eq!(chunks("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(chunks("abcdef", 3), ["abc", "def"]);
        assert_eq!(chunks("ab", 3), ["ab"]);
        assert_eq!(chunks("", 3), [""]);
    }

    #[test]
    fn counts_display_width() {
        // Each of these takes two columns.
        assert_eq!(chunks("日本語です", 4), ["日本", "語で", "す"]);
        assert_eq!(chunks("a日b", 2), ["a", "日", "b"]);
        // Wider than the limit: alone on its row rather than looping forever.
        assert_eq!(chunks("日日", 1), ["日", "日"]);
        // Combining marks take no width and stay with their base character.
        assert_eq!(chunks("e\u{301}e\u{301}", 1), ["e\u{301}", "e\u{301}"]);
    }

    #[test]
    fn tabs_expand_before_wrapping() {
        let line = DiffLine {
            kind: LineKind::Added,
            old_no: None,
            new_no: Some(1),
            text: "\tx".into(),
            fresh: false,
        };
        assert_eq!(display_text(&line), "    x");
    }

    #[test]
    fn text_width_leaves_room_for_the_gutter() {
        // " 123 456 + " is 11 columns with 3-digit line numbers.
        assert_eq!(text_width(LineKind::Added, 50, 3), 39);
        assert_eq!(text_width(LineKind::HunkHeader, 50, 3), 49);
        assert_eq!(text_width(LineKind::Context, 5, 3), 1, "never zero");
    }
}
