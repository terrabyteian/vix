use crate::mode::{Mode, PendingOp};
use crate::motion::{FindDirection, FindKind, Motion};
use crate::textobject::{TextObject, TextObjectKind};

/// A resolved editor action. Produced by the keymap after seeing enough keys
/// (count + operator + motion, or a single mode-switch key, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Move the cursor by a motion, `count` times.
    Move(Motion, usize),
    /// Apply an operator over a motion's range.
    Operate(PendingOp, Motion, usize),
    /// Apply an operator over the current line ("dd", "yy", "cc").
    OperateLine(PendingOp, usize),
    /// Apply an operator over a text object (e.g. "diw", "ci\"").
    OperateObject(PendingOp, TextObject, TextObjectKind, usize),
    /// Enter a mode.
    EnterMode(Mode),
    /// Enter Insert mode at a specific position relative to cursor.
    EnterInsert(InsertPos),
    /// Undo last change.
    Undo,
    /// Redo.
    Redo,
    /// Repeat the last editing change (`.`).
    RepeatLastChange,
    /// Paste from the unnamed register. `after` = true for `p`, false for `P`.
    Paste { after: bool, count: usize },
    /// Run an ex command (the string is everything after `:`, already entered).
    ExCommand(String),
    /// Begin entering a search pattern. Direction determines forward / backward.
    EnterSearch(SearchDirection),
    /// Jump to next search match in the given direction.
    SearchRepeat(SearchDirection),
    /// Repeat last f/F/t/T; `reverse` swaps direction (comma).
    RepeatFind { reverse: bool, count: usize },
    /// `*` / `#` — search for the word under cursor.
    WordSearchUnder(SearchDirection),
    /// `~` — toggle case of char under cursor (advances `count` chars).
    ToggleCase(usize),
    /// `x` / `X` — delete `count` chars forward (x) or backward (X), clamped
    /// to the current line. Unlike `dl`/`dh`, this correctly handles deleting
    /// the last char on a line.
    DeleteChars { forward: bool, count: usize },
    /// `K` — LSP hover at cursor.
    LspHover,
    /// `gd` — LSP goto-definition at cursor.
    LspGotoDefinition,
    /// `gA` — LSP code actions at cursor.
    LspCodeAction,
    /// `go` (and `Ctrl-O`) — walk back through the jump list.
    JumpBack,
    /// `gi` (and `Ctrl-I` / Tab) — walk forward through the jump list.
    JumpForward,
    /// Do nothing — consumed keys for a partial sequence.
    Pending,
    /// Key was not recognized in this context.
    Unhandled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDirection {
    Forward,  // /, n
    Backward, // ?, N
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertPos {
    AtCursor,    // i
    BeforeLine,  // I (first non-blank)
    AfterCursor, // a
    AfterLine,   // A (line end)
    OpenBelow,   // o
    OpenAbove,   // O
}

/// Accumulates keypresses in Normal mode. Caller resets (`take`) after each
/// resolved Action.
#[derive(Debug, Default)]
pub struct NormalKeyState {
    /// Leading count digits typed so far (e.g. "23" in "23dw").
    count_buf: String,
    /// Pending operator waiting for a motion (e.g. `d` seen, waiting for `w`).
    op: Option<PendingOp>,
    /// Count typed *after* the operator, multiplied into the final count.
    op_count_buf: String,
    /// Single-char prefix we've already consumed (e.g. `g` in `gg`).
    prefix: Option<char>,
    /// After an operator, seeing `i` or `a` puts us here — waiting for the
    /// object character (w, ", ', (, ), [, ], {, }, <, >).
    text_object_kind: Option<TextObjectKind>,
    /// After `f`/`F`/`t`/`T`, waiting for the target character.
    pending_find: Option<(FindDirection, FindKind)>,
}

impl NormalKeyState {
    /// True iff a `g` prefix has been consumed and we're awaiting its second char.
    pub fn awaiting_g(&self) -> bool {
        self.prefix == Some('g')
    }
    fn count(&self) -> usize {
        self.count_buf.parse::<usize>().ok().unwrap_or(0).max(0)
    }
    fn op_count(&self) -> usize {
        self.op_count_buf.parse::<usize>().ok().unwrap_or(0).max(0)
    }
    /// Final count = (leading count or 1) × (op count or 1).
    fn effective_count(&self) -> usize {
        let a = self.count().max(1);
        let b = self.op_count().max(1);
        a * b
    }
    fn reset(&mut self) {
        self.count_buf.clear();
        self.op = None;
        self.op_count_buf.clear();
        self.prefix = None;
        self.text_object_kind = None;
        self.pending_find = None;
    }
}

/// Feed a single character to the Normal-mode keymap.
/// Returns an Action; caller should reset state if Action is not Pending.
pub fn handle_normal_char(state: &mut NormalKeyState, c: char) -> Action {
    // If we're waiting for a find-target character, consume this key as the
    // target (even if it's a digit). Operator may also be pending.
    if let Some((dir, kind)) = state.pending_find {
        let n = state.effective_count();
        let op = state.op;
        state.reset();
        let motion = Motion::FindChar(c, dir, kind);
        return match op {
            Some(op) => Action::Operate(op, motion, n),
            None => Action::Move(motion, n),
        };
    }

    // Digit prefix accumulation (but leading 0 is `line-start` motion, not a count).
    if c.is_ascii_digit() {
        let is_leading_zero = c == '0'
            && state.op.is_none()
            && state.count_buf.is_empty()
            && state.op_count_buf.is_empty()
            && state.prefix.is_none();
        if !is_leading_zero {
            if state.op.is_some() {
                state.op_count_buf.push(c);
            } else {
                state.count_buf.push(c);
            }
            return Action::Pending;
        }
    }

    // Handle `g` prefix: next char must be `g` for `gg`, `u`/`U` for case ops.
    if let Some('g') = state.prefix {
        let n = state.count(); // raw, not effective — 0 means "first line"
        match c {
            'g' => {
                let pending_op = state.op;
                state.reset();
                return match pending_op {
                    Some(op) => Action::Operate(op, Motion::BufferStart, n),
                    None => Action::Move(Motion::BufferStart, n),
                };
            }
            'd' => {
                state.reset();
                return Action::LspGotoDefinition;
            }
            'A' => {
                state.reset();
                return Action::LspCodeAction;
            }
            'o' => {
                state.reset();
                return Action::JumpBack;
            }
            'i' => {
                state.reset();
                return Action::JumpForward;
            }
            'u' => {
                // `gu` is an operator awaiting a motion. If we're already in
                // operator-pending state (e.g. `dgu...`) cancel — that's not
                // a sensible composition.
                if state.op.is_some() {
                    state.reset();
                    return Action::Unhandled;
                }
                state.prefix = None;
                state.op = Some(PendingOp::ToLower);
                return Action::Pending;
            }
            'U' => {
                if state.op.is_some() {
                    state.reset();
                    return Action::Unhandled;
                }
                state.prefix = None;
                state.op = Some(PendingOp::ToUpper);
                return Action::Pending;
            }
            'c' => {
                if state.op.is_some() {
                    state.reset();
                    return Action::Unhandled;
                }
                state.prefix = None;
                state.op = Some(PendingOp::ToggleComment);
                return Action::Pending;
            }
            _ => {
                state.reset();
                return Action::Unhandled;
            }
        }
    }

    // Operator-pending: the current key should resolve as a motion, a
    // text-object prefix (`i`/`a`), an object character (if a prefix is
    // already pending), or be the operator character itself (doubled, for
    // linewise: `dd`, `yy`, `cc`).
    if let Some(op) = state.op {
        // If we've already consumed `i` or `a`, this key is the object.
        if let Some(kind) = state.text_object_kind {
            let obj = match c {
                'w' => Some(TextObject::Word),
                '"' => Some(TextObject::Quote('"')),
                '\'' => Some(TextObject::Quote('\'')),
                '`' => Some(TextObject::Quote('`')),
                '(' | ')' | 'b' => Some(TextObject::Pair('(', ')')),
                '{' | '}' | 'B' => Some(TextObject::Pair('{', '}')),
                '[' | ']' => Some(TextObject::Pair('[', ']')),
                '<' | '>' => Some(TextObject::Pair('<', '>')),
                _ => None,
            };
            let n = state.effective_count();
            state.reset();
            return match obj {
                Some(o) => Action::OperateObject(op, o, kind, n),
                None => Action::Unhandled,
            };
        }
        // `i` or `a` = begin text-object.
        if c == 'i' {
            state.text_object_kind = Some(TextObjectKind::Inner);
            return Action::Pending;
        }
        if c == 'a' {
            state.text_object_kind = Some(TextObjectKind::Around);
            return Action::Pending;
        }

        // f/F/t/T after operator: set up pending_find; target char comes next.
        // The early `pending_find` branch at the top handles the second key.
        match c {
            'f' => {
                state.pending_find = Some((FindDirection::Forward, FindKind::On));
                return Action::Pending;
            }
            'F' => {
                state.pending_find = Some((FindDirection::Backward, FindKind::On));
                return Action::Pending;
            }
            't' => {
                state.pending_find = Some((FindDirection::Forward, FindKind::Till));
                return Action::Pending;
            }
            'T' => {
                state.pending_find = Some((FindDirection::Backward, FindKind::Till));
                return Action::Pending;
            }
            _ => {}
        }

        // Doubled operator = operate on line.
        let doubled = matches!(
            (op, c),
            (PendingOp::Delete, 'd')
                | (PendingOp::Change, 'c')
                | (PendingOp::Yank, 'y')
                | (PendingOp::ShiftLeft, '<')
                | (PendingOp::ShiftRight, '>')
                | (PendingOp::ToggleComment, 'c')
        );
        if doubled {
            let n = state.effective_count();
            state.reset();
            return Action::OperateLine(op, n);
        }
        // `G` after an operator: linewise delete/change/yank from current line
        // through line `count` (or end-of-buffer with no count).
        if c == 'G' {
            let n = state.count();
            state.reset();
            return Action::Operate(op, Motion::BufferEnd, n);
        }
        // `g` is a prefix even in operator-pending — `gg`, `gu`, `gU` etc.
        if c == 'g' {
            state.prefix = Some('g');
            return Action::Pending;
        }
        // Otherwise interpret as motion.
        if let Some(m) = simple_motion(c) {
            let n = state.effective_count();
            state.reset();
            return Action::Operate(op, m, n);
        }
        // Unrecognized key after operator — cancel.
        state.reset();
        return Action::Unhandled;
    }

    match c {
        // Motions
        'h' => {
            let n = state.effective_count();
            state.reset();
            Action::Move(Motion::Left, n)
        }
        'l' => {
            let n = state.effective_count();
            state.reset();
            Action::Move(Motion::Right, n)
        }
        'j' => {
            let n = state.effective_count();
            state.reset();
            Action::Move(Motion::Down, n)
        }
        'k' => {
            let n = state.effective_count();
            state.reset();
            Action::Move(Motion::Up, n)
        }
        '0' => {
            state.reset();
            Action::Move(Motion::LineStart, 1)
        }
        '^' => {
            state.reset();
            Action::Move(Motion::LineFirstNonBlank, 1)
        }
        '$' => {
            state.reset();
            Action::Move(Motion::LineEnd, 1)
        }
        'w' => {
            let n = state.effective_count();
            state.reset();
            Action::Move(Motion::WordForward, n)
        }
        'b' => {
            let n = state.effective_count();
            state.reset();
            Action::Move(Motion::WordBackward, n)
        }
        'e' => {
            let n = state.effective_count();
            state.reset();
            Action::Move(Motion::WordEnd, n)
        }
        '%' => {
            state.reset();
            Action::Move(Motion::MatchBracket, 1)
        }
        'G' => {
            let n = state.count();
            state.reset();
            Action::Move(Motion::BufferEnd, n)
        }
        'g' => {
            state.prefix = Some('g');
            Action::Pending
        }

        // Mode switches
        'i' => {
            state.reset();
            Action::EnterInsert(InsertPos::AtCursor)
        }
        'I' => {
            state.reset();
            Action::EnterInsert(InsertPos::BeforeLine)
        }
        'a' => {
            state.reset();
            Action::EnterInsert(InsertPos::AfterCursor)
        }
        'A' => {
            state.reset();
            Action::EnterInsert(InsertPos::AfterLine)
        }
        'o' => {
            state.reset();
            Action::EnterInsert(InsertPos::OpenBelow)
        }
        'O' => {
            state.reset();
            Action::EnterInsert(InsertPos::OpenAbove)
        }
        'v' => {
            state.reset();
            Action::EnterMode(Mode::Visual)
        }
        'V' => {
            state.reset();
            Action::EnterMode(Mode::VisualLine)
        }
        ':' => {
            state.reset();
            Action::EnterMode(Mode::Command)
        }
        '/' => {
            state.reset();
            Action::EnterSearch(SearchDirection::Forward)
        }
        '?' => {
            state.reset();
            Action::EnterSearch(SearchDirection::Backward)
        }
        'n' => {
            state.reset();
            Action::SearchRepeat(SearchDirection::Forward)
        }
        'N' => {
            state.reset();
            Action::SearchRepeat(SearchDirection::Backward)
        }
        '*' => {
            state.reset();
            Action::WordSearchUnder(SearchDirection::Forward)
        }
        '#' => {
            state.reset();
            Action::WordSearchUnder(SearchDirection::Backward)
        }

        // Char-find motions: next keystroke is the target.
        'f' => {
            state.pending_find = Some((FindDirection::Forward, FindKind::On));
            Action::Pending
        }
        'F' => {
            state.pending_find = Some((FindDirection::Backward, FindKind::On));
            Action::Pending
        }
        't' => {
            state.pending_find = Some((FindDirection::Forward, FindKind::Till));
            Action::Pending
        }
        'T' => {
            state.pending_find = Some((FindDirection::Backward, FindKind::Till));
            Action::Pending
        }
        ';' => {
            let n = state.effective_count();
            state.reset();
            Action::RepeatFind {
                reverse: false,
                count: n,
            }
        }
        ',' => {
            let n = state.effective_count();
            state.reset();
            Action::RepeatFind {
                reverse: true,
                count: n,
            }
        }

        // Operators — begin pending.
        'd' => {
            state.op = Some(PendingOp::Delete);
            Action::Pending
        }
        'c' => {
            state.op = Some(PendingOp::Change);
            Action::Pending
        }
        'y' => {
            state.op = Some(PendingOp::Yank);
            Action::Pending
        }
        '<' => {
            state.op = Some(PendingOp::ShiftLeft);
            Action::Pending
        }
        '>' => {
            state.op = Some(PendingOp::ShiftRight);
            Action::Pending
        }

        // Undo / redo / repeat
        'u' => {
            state.reset();
            Action::Undo
        }
        '.' => {
            state.reset();
            Action::RepeatLastChange
        }

        // `~` toggles case of the char under cursor and advances. Emit as
        // a swap-case operator over a 1-char range: model it as Operate with
        // a Right motion of 0 — actually simplest is a dedicated action.
        '~' => {
            let n = state.effective_count();
            state.reset();
            Action::ToggleCase(n)
        }

        // Paste
        'p' => {
            let n = state.effective_count();
            state.reset();
            Action::Paste {
                after: true,
                count: n,
            }
        }
        'P' => {
            let n = state.effective_count();
            state.reset();
            Action::Paste {
                after: false,
                count: n,
            }
        }

        // Delete char under / before cursor. Vim's `x`/`X` are like `dl`/`dh`
        // but include the last char on a line (which Motion::Right won't cross).
        'x' => {
            let n = state.effective_count();
            state.reset();
            Action::DeleteChars {
                forward: true,
                count: n,
            }
        }
        'X' => {
            let n = state.effective_count();
            state.reset();
            Action::DeleteChars {
                forward: false,
                count: n,
            }
        }

        // LSP
        'K' => {
            state.reset();
            Action::LspHover
        }

        _ => {
            state.reset();
            Action::Unhandled
        }
    }
}

fn simple_motion(c: char) -> Option<Motion> {
    Some(match c {
        'h' => Motion::Left,
        'l' => Motion::Right,
        'j' => Motion::Down,
        'k' => Motion::Up,
        '0' => Motion::LineStart,
        '^' => Motion::LineFirstNonBlank,
        '$' => Motion::LineEnd,
        'w' => Motion::WordForward,
        'b' => Motion::WordBackward,
        'e' => Motion::WordEnd,
        '%' => Motion::MatchBracket,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_motion() {
        let mut s = NormalKeyState::default();
        assert_eq!(
            handle_normal_char(&mut s, 'l'),
            Action::Move(Motion::Right, 1)
        );
    }

    #[test]
    fn count_prefix() {
        let mut s = NormalKeyState::default();
        assert_eq!(handle_normal_char(&mut s, '3'), Action::Pending);
        assert_eq!(
            handle_normal_char(&mut s, 'w'),
            Action::Move(Motion::WordForward, 3)
        );
    }

    #[test]
    fn operator_then_motion() {
        let mut s = NormalKeyState::default();
        assert_eq!(handle_normal_char(&mut s, 'd'), Action::Pending);
        assert_eq!(
            handle_normal_char(&mut s, 'w'),
            Action::Operate(PendingOp::Delete, Motion::WordForward, 1)
        );
    }

    #[test]
    fn doubled_operator_is_linewise() {
        let mut s = NormalKeyState::default();
        assert_eq!(handle_normal_char(&mut s, 'd'), Action::Pending);
        assert_eq!(
            handle_normal_char(&mut s, 'd'),
            Action::OperateLine(PendingOp::Delete, 1)
        );
    }

    #[test]
    fn count_times_count() {
        let mut s = NormalKeyState::default();
        // 2d3w = delete 6 words
        assert_eq!(handle_normal_char(&mut s, '2'), Action::Pending);
        assert_eq!(handle_normal_char(&mut s, 'd'), Action::Pending);
        assert_eq!(handle_normal_char(&mut s, '3'), Action::Pending);
        assert_eq!(
            handle_normal_char(&mut s, 'w'),
            Action::Operate(PendingOp::Delete, Motion::WordForward, 6)
        );
    }

    #[test]
    fn leading_zero_is_line_start() {
        let mut s = NormalKeyState::default();
        assert_eq!(
            handle_normal_char(&mut s, '0'),
            Action::Move(Motion::LineStart, 1)
        );
    }

    #[test]
    fn zero_after_digit_is_count() {
        let mut s = NormalKeyState::default();
        assert_eq!(handle_normal_char(&mut s, '1'), Action::Pending);
        assert_eq!(handle_normal_char(&mut s, '0'), Action::Pending);
        assert_eq!(
            handle_normal_char(&mut s, 'l'),
            Action::Move(Motion::Right, 10)
        );
    }

    #[test]
    fn gg_goes_to_buffer_start() {
        let mut s = NormalKeyState::default();
        assert_eq!(handle_normal_char(&mut s, 'g'), Action::Pending);
        // count 0 = "first line" default
        assert_eq!(
            handle_normal_char(&mut s, 'g'),
            Action::Move(Motion::BufferStart, 0)
        );
    }

    #[test]
    fn line_jump_with_count() {
        let mut s = NormalKeyState::default();
        assert_eq!(handle_normal_char(&mut s, '5'), Action::Pending);
        assert_eq!(
            handle_normal_char(&mut s, 'G'),
            Action::Move(Motion::BufferEnd, 5)
        );
    }
}
