//! Regex search over a Buffer.
//!
//! Strategy for v1: scan line by line. Simple, correct, and adequate for
//! typical file sizes. Multi-line patterns and very large files can later
//! be served by `regex-cursor` streaming over rope chunks.

use crate::buffer::Buffer;
use regex::Regex;
use std::borrow::Cow;

/// Case sensitivity strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Case {
    /// Respect literal casing of the query.
    Sensitive,
    #[default]
    /// Case-insensitive if the query is all lowercase; sensitive otherwise.
    Smart,
}

pub fn compile(pattern: &str, case: Case) -> Result<Regex, regex::Error> {
    let is_lower = pattern.chars().all(|c| !c.is_uppercase());
    let case_insensitive = matches!(case, Case::Smart) && is_lower;
    regex::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .build()
}

/// Find the next match at or after `from` (char offset).
/// Returns (start_char, end_char) if found.
pub fn find_forward(buf: &Buffer, re: &Regex, from: usize) -> Option<(usize, usize)> {
    let total_lines = buf.len_lines();
    let (start_line, start_col) = buf.char_to_line_col(from);
    for line in start_line..total_lines {
        let line_slice = buf.rope().line(line);
        let line_text: Cow<str> = line_slice
            .as_str()
            .map(Cow::Borrowed)
            .unwrap_or_else(|| Cow::Owned(line_slice.chars().collect()));
        // Search from `col` on the first line, from 0 on subsequent lines.
        let search_from_bytes = if line == start_line {
            // Convert start_col (char) to byte offset within line_text.
            line_text
                .char_indices()
                .nth(start_col)
                .map(|(b, _)| b)
                .unwrap_or(line_text.len())
        } else {
            0
        };
        if search_from_bytes > line_text.len() {
            continue;
        }
        // `find_at` rather than slicing: anchors and `\b` need to see the
        // text before the start offset, or `^foo` matches mid-line.
        if let Some(m) = re.find_at(&line_text, search_from_bytes) {
            let line_start = buf.line_to_char(line);
            // Convert byte offsets within the line back to char offsets within the buffer.
            let start_char = line_start + byte_to_char_in(&line_text, m.start());
            let end_char = line_start + byte_to_char_in(&line_text, m.end());
            return Some((start_char, end_char));
        }
    }
    None
}

/// Find the previous match strictly before `from` (char offset).
pub fn find_backward(buf: &Buffer, re: &Regex, from: usize) -> Option<(usize, usize)> {
    let (start_line, start_col) = buf.char_to_line_col(from);
    for line in (0..=start_line).rev() {
        let line_slice = buf.rope().line(line);
        let line_text: Cow<str> = line_slice
            .as_str()
            .map(Cow::Borrowed)
            .unwrap_or_else(|| Cow::Owned(line_slice.chars().collect()));
        let max_byte = if line == start_line {
            line_text
                .char_indices()
                .nth(start_col)
                .map(|(b, _)| b)
                .unwrap_or(line_text.len())
        } else {
            line_text.len()
        };
        if max_byte == 0 {
            continue;
        }
        // Last match starting before `max_byte`. Matched against the whole
        // line, not a slice cut at the cursor: a cut would let `$` and `\b`
        // match at the cut point.
        let last = re
            .find_iter(&line_text)
            .take_while(|m| m.start() < max_byte)
            .last();
        if let Some(m) = last {
            let line_start = buf.line_to_char(line);
            let start_char = line_start + byte_to_char_in(&line_text, m.start());
            let end_char = line_start + byte_to_char_in(&line_text, m.end());
            return Some((start_char, end_char));
        }
    }
    None
}

/// Collect all matches within a range of lines (for highlighting the viewport).
pub fn find_all_in_lines(
    buf: &Buffer,
    re: &Regex,
    start_line: usize,
    end_line: usize,
) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let last = end_line.min(buf.len_lines());
    for line in start_line..last {
        let line_slice = buf.rope().line(line);
        let line_text: Cow<str> = line_slice
            .as_str()
            .map(Cow::Borrowed)
            .unwrap_or_else(|| Cow::Owned(line_slice.chars().collect()));
        let line_start = buf.line_to_char(line);
        for m in re.find_iter(&line_text) {
            let s = line_start + byte_to_char_in(&line_text, m.start());
            let e = line_start + byte_to_char_in(&line_text, m.end());
            out.push((s, e));
        }
    }
    out
}

fn byte_to_char_in(s: &str, byte: usize) -> usize {
    s[..byte.min(s.len())].chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_case_lowercase_matches_any_case() {
        let re = compile("foo", Case::Smart).unwrap();
        assert!(re.is_match("FOO"));
        assert!(re.is_match("foo"));
    }

    #[test]
    fn smart_case_with_uppercase_is_sensitive() {
        let re = compile("Foo", Case::Smart).unwrap();
        assert!(re.is_match("Foo"));
        assert!(!re.is_match("foo"));
    }

    #[test]
    fn forward_match_in_same_line() {
        let buf = Buffer::from_text("hello world\nfoo bar\n");
        let re = compile("world", Case::Smart).unwrap();
        assert_eq!(find_forward(&buf, &re, 0), Some((6, 11)));
    }

    #[test]
    fn forward_wraps_across_lines() {
        let buf = Buffer::from_text("aaa\nbbb foo\nccc");
        let re = compile("foo", Case::Smart).unwrap();
        assert_eq!(find_forward(&buf, &re, 0), Some((8, 11)));
    }

    #[test]
    fn forward_starting_mid_line() {
        let buf = Buffer::from_text("foo bar foo");
        let re = compile("foo", Case::Smart).unwrap();
        // Starting after first match should find the second.
        assert_eq!(find_forward(&buf, &re, 3), Some((8, 11)));
    }

    #[test]
    fn backward_match() {
        let buf = Buffer::from_text("foo bar foo baz");
        let re = compile("foo", Case::Smart).unwrap();
        assert_eq!(find_backward(&buf, &re, 14), Some((8, 11)));
        assert_eq!(find_backward(&buf, &re, 8), Some((0, 3)));
    }

    #[test]
    fn forward_keeps_anchor_context_before_the_start_offset() {
        let buf = Buffer::from_text("abar bar\nxfoo\nfoo\n");
        let word = compile(r"\bbar\b", Case::Smart).unwrap();
        // From inside "abar": the standalone "bar", not the tail of "abar".
        assert_eq!(find_forward(&buf, &word, 1), Some((5, 8)));
        let anchored = compile("^foo", Case::Smart).unwrap();
        // From col 1 of "xfoo": the next line, not mid-line.
        assert_eq!(find_forward(&buf, &anchored, 10), Some((14, 17)));
    }

    #[test]
    fn backward_keeps_anchor_context_after_the_cursor() {
        let buf = Buffer::from_text("foo foobar");
        let word = compile(r"foo\b", Case::Smart).unwrap();
        // Cursor right after the "foo" of "foobar": only the first is a word.
        assert_eq!(find_backward(&buf, &word, 7), Some((0, 3)));
    }

    #[test]
    fn find_all_in_lines_collects() {
        let buf = Buffer::from_text("foo\nbar foo\nfoo");
        let re = compile("foo", Case::Smart).unwrap();
        let all = find_all_in_lines(&buf, &re, 0, 3);
        assert_eq!(all.len(), 3);
    }
}
