use crate::buffer::Buffer;
use crate::selection::Selection;

/// Compute a new Selection given the current Selection, buffer, and motion.
/// Motions only move the cursor — they do not mutate the buffer.
pub fn apply(buf: &Buffer, sel: Selection, motion: Motion, count: usize) -> Selection {
    let n = count.max(1);
    match motion {
        Motion::Left => left(buf, sel, n),
        Motion::Right => right(buf, sel, n),
        Motion::Up => vertical(buf, sel, -(n as isize)),
        Motion::Down => vertical(buf, sel, n as isize),
        Motion::LineStart => line_start(buf, sel),
        Motion::LineFirstNonBlank => line_first_non_blank(buf, sel),
        Motion::LineEnd => line_end(buf, sel),
        // For BufferStart/End, raw count (0 = default, >0 = line number 1-indexed).
        Motion::BufferStart => goto_line(buf, sel, count, 0),
        Motion::BufferEnd => goto_line(buf, sel, count, buf.len_lines().saturating_sub(1)),
        Motion::WordForward => word_forward(buf, sel, n),
        Motion::WordBackward => word_backward(buf, sel, n),
        Motion::WordEnd => word_end(buf, sel, n),
        Motion::FindChar(c, dir, kind) => find_char(buf, sel, c, dir, kind, n),
        Motion::MatchBracket => match_bracket(buf, sel),
    }
}

fn match_bracket(buf: &Buffer, sel: Selection) -> Selection {
    let rope = buf.rope();
    let len = buf.len_chars();
    if len == 0 {
        return sel;
    }

    // 1. Find the bracket to start from — scan from cursor to end-of-line.
    let (line, _col) = buf.char_to_line_col(sel.head);
    let line_end = buf.line_to_char(line) + buf.line_len_chars(line);
    let mut start_pos = None;
    for i in sel.head..line_end {
        if is_bracket(rope.char(i)) {
            start_pos = Some(i);
            break;
        }
    }
    let Some(start) = start_pos else {
        return sel;
    };
    let ch = rope.char(start);
    let (open, close, forward) = match ch {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        '<' => ('<', '>', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        '>' => ('<', '>', false),
        _ => return sel,
    };

    let target = if forward {
        // Scan right counting nesting.
        let mut depth = 0i32;
        let mut i = start + 1;
        let mut found = None;
        while i < len {
            let c = rope.char(i);
            if c == open {
                depth += 1;
            } else if c == close {
                if depth == 0 {
                    found = Some(i);
                    break;
                }
                depth -= 1;
            }
            i += 1;
        }
        found
    } else {
        let mut depth = 0i32;
        let mut i = start;
        let mut found = None;
        loop {
            if i == 0 {
                break;
            }
            i -= 1;
            let c = rope.char(i);
            if c == close {
                depth += 1;
            } else if c == open {
                if depth == 0 {
                    found = Some(i);
                    break;
                }
                depth -= 1;
            }
        }
        found
    };

    match target {
        Some(t) => sel.move_to(t).with_virt_col(None),
        None => sel,
    }
}

fn is_bracket(c: char) -> bool {
    matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>')
}

fn goto_line(buf: &Buffer, sel: Selection, count: usize, default_line: usize) -> Selection {
    let last = buf.len_lines().saturating_sub(1);
    let line = if count == 0 {
        default_line
    } else {
        count.saturating_sub(1).min(last)
    };
    let pos = buf.line_to_char(line);
    let sel = sel.move_to(pos).with_virt_col(None).clamped(buf);
    // Vim lands on first non-blank of the target line.
    line_first_non_blank(buf, sel)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,         // 0
    LineFirstNonBlank, // ^
    LineEnd,           // $
    BufferStart,       // gg
    BufferEnd,         // G
    WordForward,       // w
    WordBackward,      // b
    WordEnd,           // e
    /// `f`/`F`/`t`/`T` char-find on the current line.
    FindChar(char, FindDirection, FindKind),
    /// `%` — jump to matching bracket. Finds the nearest bracket on the line
    /// at or after the cursor, then jumps to its mate, honoring nesting.
    MatchBracket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindKind {
    /// `f`/`F` — land ON the target char.
    On,
    /// `t`/`T` — land JUST BEFORE the target (or after for backward).
    Till,
}

fn left(buf: &Buffer, sel: Selection, n: usize) -> Selection {
    let (line, col) = buf.char_to_line_col(sel.head);
    let new_col = col.saturating_sub(n);
    let new_pos = buf.line_to_char(line) + new_col;
    sel.move_to(new_pos).with_virt_col(None)
}

fn right(buf: &Buffer, sel: Selection, n: usize) -> Selection {
    let (line, col) = buf.char_to_line_col(sel.head);
    let line_len = buf.line_len_chars(line);
    // Vim: cursor stops at the last char of the line in Normal mode, not past it.
    let new_col = (col + n).min(line_len.saturating_sub(1));
    let new_pos = buf.line_to_char(line) + new_col;
    sel.move_to(new_pos).with_virt_col(None)
}

fn vertical(buf: &Buffer, sel: Selection, delta: isize) -> Selection {
    let (line, col) = buf.char_to_line_col(sel.head);
    let virt = sel.virt_col.unwrap_or(col);
    let last_line = buf.len_lines().saturating_sub(1);
    let new_line = (line as isize + delta).clamp(0, last_line as isize) as usize;
    let line_len = buf.line_len_chars(new_line);
    let new_col = virt.min(line_len.saturating_sub(1));
    let new_pos = buf.line_to_char(new_line) + new_col;
    Selection {
        anchor: new_pos,
        head: new_pos,
        virt_col: Some(virt),
    }
}

fn line_start(buf: &Buffer, sel: Selection) -> Selection {
    let (line, _) = buf.char_to_line_col(sel.head);
    sel.move_to(buf.line_to_char(line)).with_virt_col(None)
}

fn line_first_non_blank(buf: &Buffer, sel: Selection) -> Selection {
    let (line, _) = buf.char_to_line_col(sel.head);
    let start = buf.line_to_char(line);
    let line_len = buf.line_len_chars(line);
    let rope = buf.rope();
    let mut p = start;
    let end = start + line_len;
    while p < end {
        let c = rope.char(p);
        if c != ' ' && c != '\t' {
            break;
        }
        p += 1;
    }
    sel.move_to(p).with_virt_col(None)
}

fn line_end(buf: &Buffer, sel: Selection) -> Selection {
    let (line, _) = buf.char_to_line_col(sel.head);
    let line_len = buf.line_len_chars(line);
    let new_pos = buf.line_to_char(line) + line_len.saturating_sub(1);
    sel.move_to(new_pos).with_virt_col(None)
}

// --- word motions -----------------------------------------------------------
// Vim's `w`, `b`, `e` operate on word "classes":
//   - word chars: alphanumerics + underscore
//   - punctuation: other non-whitespace
//   - whitespace
// `w` skips to the start of the next word (class-boundary transition).

#[derive(PartialEq, Eq)]
enum CharClass {
    Word,
    Punct,
    Space,
    Newline,
}

fn classify(c: char) -> CharClass {
    if c == '\n' {
        CharClass::Newline
    } else if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

fn word_forward(buf: &Buffer, sel: Selection, n: usize) -> Selection {
    let mut pos = sel.head;
    let len = buf.len_chars();
    for _ in 0..n {
        if pos >= len {
            break;
        }
        let start_class = classify(buf.rope().char(pos));
        // Advance through current class.
        while pos < len && classify(buf.rope().char(pos)) == start_class {
            pos += 1;
        }
        // Skip whitespace (but stop at newlines — Vim treats blank lines as words).
        while pos < len {
            let c = buf.rope().char(pos);
            if c == '\n' || !c.is_whitespace() {
                break;
            }
            pos += 1;
        }
    }
    sel.move_to(pos.min(len.saturating_sub(1)))
        .with_virt_col(None)
}

fn word_backward(buf: &Buffer, sel: Selection, n: usize) -> Selection {
    let mut pos = sel.head;
    for _ in 0..n {
        if pos == 0 {
            break;
        }
        pos -= 1;
        // Skip whitespace backward (not newlines).
        while pos > 0 {
            let c = buf.rope().char(pos);
            if c == '\n' || !c.is_whitespace() {
                break;
            }
            pos -= 1;
        }
        if pos == 0 {
            break;
        }
        let end_class = classify(buf.rope().char(pos));
        while pos > 0 {
            let prev = buf.rope().char(pos - 1);
            if classify(prev) != end_class {
                break;
            }
            pos -= 1;
        }
    }
    sel.move_to(pos).with_virt_col(None)
}

fn word_end(buf: &Buffer, sel: Selection, n: usize) -> Selection {
    let mut pos = sel.head;
    let len = buf.len_chars();
    for _ in 0..n {
        if pos + 1 >= len {
            break;
        }
        pos += 1;
        // Skip whitespace forward.
        while pos < len {
            let c = buf.rope().char(pos);
            if c == '\n' || !c.is_whitespace() {
                break;
            }
            pos += 1;
        }
        if pos >= len {
            break;
        }
        let class = classify(buf.rope().char(pos));
        while pos + 1 < len && classify(buf.rope().char(pos + 1)) == class {
            pos += 1;
        }
    }
    sel.move_to(pos.min(len.saturating_sub(1)))
        .with_virt_col(None)
}

fn find_char(
    buf: &Buffer,
    sel: Selection,
    target: char,
    dir: FindDirection,
    kind: FindKind,
    n: usize,
) -> Selection {
    let (line, col) = buf.char_to_line_col(sel.head);
    let line_start = buf.line_to_char(line);
    let line_len = buf.line_len_chars(line);
    let rope = buf.rope();

    match dir {
        FindDirection::Forward => {
            let mut found = 0;
            let mut target_col = None;
            // Search strictly to the right of the cursor.
            for i in (col + 1)..line_len {
                if rope.char(line_start + i) == target {
                    found += 1;
                    if found == n {
                        target_col = Some(i);
                        break;
                    }
                }
            }
            match target_col {
                Some(c) => {
                    let land = match kind {
                        FindKind::On => c,
                        FindKind::Till => c.saturating_sub(1),
                    };
                    sel.move_to(line_start + land).with_virt_col(None)
                }
                None => sel,
            }
        }
        FindDirection::Backward => {
            let mut found = 0;
            let mut target_col = None;
            if col > 0 {
                for i in (0..col).rev() {
                    if rope.char(line_start + i) == target {
                        found += 1;
                        if found == n {
                            target_col = Some(i);
                            break;
                        }
                    }
                }
            }
            match target_col {
                Some(c) => {
                    let land = match kind {
                        FindKind::On => c,
                        FindKind::Till => c + 1,
                    };
                    sel.move_to(line_start + land).with_virt_col(None)
                }
                None => sel,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(s: &str, pos: usize) -> (Buffer, Selection) {
        (Buffer::from_text(s), Selection::at(pos))
    }

    #[test]
    fn horizontal() {
        let (b, s) = setup("hello", 2);
        assert_eq!(apply(&b, s, Motion::Left, 1).head, 1);
        assert_eq!(apply(&b, s, Motion::Right, 1).head, 3);
        // Right stops at last char (index 4, "o").
        assert_eq!(apply(&b, s, Motion::Right, 10).head, 4);
        // Left stops at 0.
        assert_eq!(apply(&b, s, Motion::Left, 10).head, 0);
    }

    #[test]
    fn line_motions() {
        let (b, s) = setup("   hello", 5);
        assert_eq!(apply(&b, s, Motion::LineStart, 1).head, 0);
        assert_eq!(apply(&b, s, Motion::LineFirstNonBlank, 1).head, 3);
        assert_eq!(apply(&b, s, Motion::LineEnd, 1).head, 7);
    }

    #[test]
    fn vertical_preserves_virt_col() {
        let (b, s) = setup("aaaa\nbb\ncccc", 3); // col 3 on line 0
        let s2 = apply(&b, s, Motion::Down, 1);
        // Line 1 is "bb" (len 2), so cursor lands at col 1 (last char).
        assert_eq!(b.char_to_line_col(s2.head), (1, 1));
        let s3 = apply(&b, s2, Motion::Down, 1);
        // Line 2 is "cccc" — virt_col 3 should restore.
        assert_eq!(b.char_to_line_col(s3.head), (2, 3));
    }

    #[test]
    fn word_forward_basic() {
        let (b, s) = setup("foo bar baz", 0);
        let s = apply(&b, s, Motion::WordForward, 1);
        assert_eq!(s.head, 4); // start of "bar"
        let s = apply(&b, s, Motion::WordForward, 1);
        assert_eq!(s.head, 8); // start of "baz"
    }

    #[test]
    fn word_forward_punct() {
        let (b, s) = setup("foo.bar", 0);
        let s = apply(&b, s, Motion::WordForward, 1);
        assert_eq!(s.head, 3); // at "."
        let s = apply(&b, s, Motion::WordForward, 1);
        assert_eq!(s.head, 4); // start of "bar"
    }

    #[test]
    fn word_backward() {
        let (b, s) = setup("foo bar baz", 10);
        let s = apply(&b, s, Motion::WordBackward, 1);
        assert_eq!(s.head, 8);
        let s = apply(&b, s, Motion::WordBackward, 1);
        assert_eq!(s.head, 4);
    }

    #[test]
    fn word_end() {
        let (b, s) = setup("foo bar", 0);
        let s = apply(&b, s, Motion::WordEnd, 1);
        assert_eq!(s.head, 2); // last char of "foo"
        let s = apply(&b, s, Motion::WordEnd, 1);
        assert_eq!(s.head, 6); // last char of "bar"
    }

    #[test]
    fn find_char_forward() {
        let (b, s) = setup("abcXdef", 0);
        let s2 = apply(
            &b,
            s,
            Motion::FindChar('X', FindDirection::Forward, FindKind::On),
            1,
        );
        assert_eq!(s2.head, 3);
        let s3 = apply(
            &b,
            s,
            Motion::FindChar('X', FindDirection::Forward, FindKind::Till),
            1,
        );
        assert_eq!(s3.head, 2);
    }

    #[test]
    fn find_char_backward() {
        let (b, s) = setup("abcXdef", 6);
        let s2 = apply(
            &b,
            s,
            Motion::FindChar('X', FindDirection::Backward, FindKind::On),
            1,
        );
        assert_eq!(s2.head, 3);
        let s3 = apply(
            &b,
            s,
            Motion::FindChar('X', FindDirection::Backward, FindKind::Till),
            1,
        );
        assert_eq!(s3.head, 4);
    }

    #[test]
    fn find_char_count() {
        let (b, s) = setup("aXbXcXd", 0);
        let s = apply(
            &b,
            s,
            Motion::FindChar('X', FindDirection::Forward, FindKind::On),
            2,
        );
        assert_eq!(s.head, 3);
    }

    #[test]
    fn match_bracket_forward() {
        let (b, s) = setup("fn foo() {}", 6);
        let s = apply(&b, s, Motion::MatchBracket, 1);
        assert_eq!(s.head, 7); // ')'
    }

    #[test]
    fn match_bracket_backward() {
        let (b, s) = setup("fn foo() {}", 7);
        let s = apply(&b, s, Motion::MatchBracket, 1);
        assert_eq!(s.head, 6); // '('
    }

    #[test]
    fn match_bracket_nested() {
        let (b, s) = setup("a(b(c)d)e", 0);
        let s = apply(&b, s, Motion::MatchBracket, 1);
        assert_eq!(s.head, 7); // matches outer ')'
    }

    #[test]
    fn find_char_line_scoped() {
        let (b, s) = setup("aaa\nXbb", 0);
        // Forward find on 'X' should NOT cross the newline.
        let s2 = apply(
            &b,
            s,
            Motion::FindChar('X', FindDirection::Forward, FindKind::On),
            1,
        );
        assert_eq!(s2.head, 0); // no move
    }
}
