//! Text objects: `iw`/`aw`, `i"/a"`, `i'/a'`, `i(/a(`, `i{/a{`, `i[/a[`.
//!
//! Given the cursor position, `range_of` returns the buffer char-range of
//! the object. `Inner` excludes delimiters/surrounding whitespace; `Around`
//! includes them (and, for words, the trailing whitespace).

use crate::buffer::Buffer;

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

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_range(buf: &Buffer, pos: usize, kind: TextObjectKind) -> Option<std::ops::Range<usize>> {
    let len = buf.len_chars();
    if len == 0 {
        return None;
    }
    let rope = buf.rope();
    let pos = pos.min(len.saturating_sub(1));
    let c = rope.char(pos);
    if !is_word_char(c) && kind == TextObjectKind::Inner {
        // Inner on non-word = just that char (Vim's behavior for punctuation clusters).
        return Some(pos..(pos + 1));
    }

    // Extend left over word chars.
    let mut start = pos;
    while start > 0 && is_word_char(rope.char(start - 1)) {
        start -= 1;
    }
    // Extend right over word chars.
    let mut end = pos;
    while end < len && is_word_char(rope.char(end)) {
        end += 1;
    }

    if kind == TextObjectKind::Around {
        // Include trailing whitespace (but not past newline).
        while end < len {
            let ch = rope.char(end);
            if ch == '\n' || !ch.is_whitespace() {
                break;
            }
            end += 1;
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
