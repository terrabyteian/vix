//! Text objects: `iw`/`aw`, `i"/a"`, `i'/a'`, `i(/a(`, `i{/a{`, `i[/a[`.
//!
//! Given the cursor position, `range_of` returns the buffer char-range of
//! the object. `Inner` excludes delimiters/surrounding whitespace; `Around`
//! includes them (and, for words, the trailing whitespace).

use crate::buffer::Buffer;
use crate::motion::{classify, CharClass};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObjectKind {
    Inner,
    Around,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObject {
    Word,
    /// Paired delimiters: (), [], {}, <>.
    Pair(char, char),
    /// Symmetric quote characters: ", ', `.
    Quote(char),
}

pub fn range_of(
    buf: &Buffer,
    pos: usize,
    obj: TextObject,
    kind: TextObjectKind,
) -> Option<std::ops::Range<usize>> {
    match obj {
        TextObject::Word => word_range(buf, pos, kind),
        TextObject::Pair(open, close) => pair_range(buf, pos, open, close, kind),
        TextObject::Quote(q) => quote_range(buf, pos, q, kind),
    }
}

/// Vim's `iw`/`aw`: the run of same-class chars under the cursor — a word,
/// a punctuation cluster, or a stretch of blanks — never crossing a line
/// boundary. `Around` adds the blanks after it (or, when there are none,
/// the blanks before); on blanks it adds the following word instead.
fn word_range(buf: &Buffer, pos: usize, kind: TextObjectKind) -> Option<std::ops::Range<usize>> {
    let len = buf.len_chars();
    if len == 0 {
        return None;
    }
    let rope = buf.rope();
    let pos = pos.min(len - 1);
    let class = classify(rope.char(pos));
    // On a line break there is no word; returning the `\n` itself would
    // make `diw` on an empty line join it with the next.
    if class == CharClass::Newline {
        return None;
    }
    let run_start = |mut i: usize, class: CharClass| {
        while i > 0 && classify(rope.char(i - 1)) == class {
            i -= 1;
        }
        i
    };
    let run_end = |mut i: usize, class: CharClass| {
        while i < len && classify(rope.char(i)) == class {
            i += 1;
        }
        i
    };
    let mut start = run_start(pos, class);
    let mut end = run_end(pos, class);

    if kind == TextObjectKind::Around {
        if class == CharClass::Space {
            if end < len {
                let next = classify(rope.char(end));
                if next != CharClass::Newline {
                    end = run_end(end, next);
                }
            }
        } else {
            let with_trailing = run_end(end, CharClass::Space);
            if with_trailing > end {
                end = with_trailing;
            } else {
                start = run_start(start, CharClass::Space);
            }
        }
    }
    Some(start..end)
}

fn pair_range(
    buf: &Buffer,
    pos: usize,
    open: char,
    close: char,
    kind: TextObjectKind,
) -> Option<std::ops::Range<usize>> {
    let len = buf.len_chars();
    if len == 0 {
        return None;
    }
    let rope = buf.rope();
    // The cursor can sit one past the last char (the empty line after a
    // trailing newline, e.g. after `G`); `rope.char` panics there.
    let pos = pos.min(len - 1);
    // Scan backward for matching `open`, counting nesting.
    let mut open_at: Option<usize> = None;
    {
        let mut depth = 0i32;
        let mut i = pos;
        loop {
            let c = rope.char(i);
            if c == close && i != pos {
                depth += 1;
            } else if c == open {
                if depth == 0 {
                    open_at = Some(i);
                    break;
                }
                depth -= 1;
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }
    }
    let open_at = open_at?;

    // Scan forward for matching `close`.
    let mut close_at: Option<usize> = None;
    {
        let mut depth = 0i32;
        let mut i = open_at + 1;
        while i < len {
            let c = rope.char(i);
            if c == open {
                depth += 1;
            } else if c == close {
                if depth == 0 {
                    close_at = Some(i);
                    break;
                }
                depth -= 1;
            }
            i += 1;
        }
    }
    let close_at = close_at?;

    match kind {
        TextObjectKind::Inner => Some((open_at + 1)..close_at),
        TextObjectKind::Around => Some(open_at..(close_at + 1)),
    }
}

fn quote_range(
    buf: &Buffer,
    pos: usize,
    q: char,
    kind: TextObjectKind,
) -> Option<std::ops::Range<usize>> {
    let len = buf.len_chars();
    if len == 0 {
        return None;
    }
    let rope = buf.rope();
    // Restrict to the current line to match Vim's line-scoped quote objects.
    let (line, _) = buf.char_to_line_col(pos);
    let line_start = buf.line_to_char(line);
    let line_end = line_start + buf.line_len_chars(line);

    // Collect unescaped quote positions on the line.
    let mut quotes: Vec<usize> = Vec::new();
    let mut i = line_start;
    while i < line_end {
        let c = rope.char(i);
        if c == q {
            let escaped = i > line_start && rope.char(i - 1) == '\\';
            if !escaped {
                quotes.push(i);
            }
        }
        i += 1;
    }
    if quotes.len() < 2 {
        return None;
    }

    // Find the enclosing pair:
    //   - If cursor is on a quote and index is odd (end-of-string quote), pair with prev.
    //   - Else find first pair where pos is between them.
    for pair in quotes.chunks(2) {
        if pair.len() == 2 && pair[0] <= pos && pos <= pair[1] {
            let (a, b) = (pair[0], pair[1]);
            return Some(match kind {
                TextObjectKind::Inner => (a + 1)..b,
                TextObjectKind::Around => a..(b + 1),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inner_word() {
        let b = Buffer::from_text("foo bar baz");
        // Cursor on "bar".
        assert_eq!(
            range_of(&b, 5, TextObject::Word, TextObjectKind::Inner),
            Some(4..7)
        );
    }

    #[test]
    fn around_word_takes_trailing_space() {
        let b = Buffer::from_text("foo bar baz");
        assert_eq!(
            range_of(&b, 5, TextObject::Word, TextObjectKind::Around),
            Some(4..8)
        );
    }

    #[test]
    fn word_objects_follow_char_classes() {
        let b = Buffer::from_text("a->b  c\n\nd");
        let word = |pos, kind| range_of(&b, pos, TextObject::Word, kind);
        // A punctuation cluster is one word.
        assert_eq!(word(1, TextObjectKind::Inner), Some(1..3));
        // `aw` with no trailing blanks stays on the cursor's own run.
        assert_eq!(word(1, TextObjectKind::Around), Some(1..3));
        // On blanks: `iw` is the blanks, `aw` adds the next word.
        assert_eq!(word(4, TextObjectKind::Inner), Some(4..6));
        assert_eq!(word(4, TextObjectKind::Around), Some(4..7));
        // Last word of a line: `aw` takes the blanks before it instead.
        assert_eq!(word(6, TextObjectKind::Around), Some(4..7));
        // Nothing to select on a line break — never the newline itself.
        assert_eq!(word(7, TextObjectKind::Inner), None);
        assert_eq!(word(8, TextObjectKind::Around), None);
    }

    #[test]
    fn pair_at_end_of_buffer_does_not_panic() {
        let b = Buffer::from_text("f(x)\n");
        // Cursor on the phantom line past the trailing newline.
        assert_eq!(
            range_of(&b, 5, TextObject::Pair('(', ')'), TextObjectKind::Inner),
            None
        );
    }

    #[test]
    fn inner_parens() {
        let b = Buffer::from_text("foo(hello)bar");
        // Cursor inside "hello".
        assert_eq!(
            range_of(&b, 5, TextObject::Pair('(', ')'), TextObjectKind::Inner),
            Some(4..9)
        );
        assert_eq!(
            range_of(&b, 5, TextObject::Pair('(', ')'), TextObjectKind::Around),
            Some(3..10)
        );
    }

    #[test]
    fn nested_parens() {
        let b = Buffer::from_text("a(b(c)d)e");
        // Cursor on 'c'. Inner should match the innermost pair.
        assert_eq!(
            range_of(&b, 4, TextObject::Pair('(', ')'), TextObjectKind::Inner),
            Some(4..5)
        );
    }

    #[test]
    fn inner_double_quote() {
        let b = Buffer::from_text("foo \"hello\" bar");
        // Cursor inside "hello".
        assert_eq!(
            range_of(&b, 7, TextObject::Quote('"'), TextObjectKind::Inner),
            Some(5..10)
        );
        assert_eq!(
            range_of(&b, 7, TextObject::Quote('"'), TextObjectKind::Around),
            Some(4..11)
        );
    }

    #[test]
    fn quote_respects_escape() {
        let b = Buffer::from_text(r#"x "a\"b" y"#);
        // Quotes are at char positions 2 and 7 (the \" is escaped).
        assert_eq!(
            range_of(&b, 4, TextObject::Quote('"'), TextObjectKind::Inner),
            Some(3..7)
        );
    }

    #[test]
    fn quote_line_scoped() {
        let b = Buffer::from_text("a\nb\nc");
        // No quotes on line — should return None.
        assert_eq!(
            range_of(&b, 2, TextObject::Quote('"'), TextObjectKind::Inner),
            None
        );
    }
}
