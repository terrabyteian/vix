//! Edit dispatch: turning a resolved `Action` into buffer mutations —
//! operators, insert-session recording, paste, and dot-repeat.

use std::time::{Duration, Instant};

use vix_core::{
    apply_motion, text_object_range, Action, Buffer, Change, FindDirection, FindKind, InsertPos,
    Mode, Motion, PendingOp, RepeatAction, SearchDirection, Selection, Transaction,
};

use crate::util::osc52_copy;
use crate::{Editor, InsertOrigin, PendingInsert, Register};

impl Editor {
    /// Dispatch a resolved Action. Mutates editor state.
    pub(crate) fn dispatch(&mut self, action: Action) {
        match action {
            Action::Move(m, n) => {
                if let Motion::FindChar(c, dir, kind) = m {
                    self.last_find = Some((c, dir, kind));
                }
                // gg / G / nG are jump-listed.
                if matches!(m, Motion::BufferStart | Motion::BufferEnd) {
                    self.push_jump();
                }
                let new_sel = apply_motion(&self.buffer, self.sel, m, n);
                if self.mode == Mode::Visual || self.mode == Mode::VisualLine {
                    // Extend: keep anchor, move head only.
                    self.sel = Selection {
                        anchor: self.sel.anchor,
                        head: new_sel.head,
                        virt_col: new_sel.virt_col,
                    };
                } else {
                    self.sel = new_sel;
                }
                self.sel = self.sel.clamped(&self.buffer);
            }
            Action::EnterMode(m) => {
                if m == Mode::Command {
                    self.cmdline.clear();
                }
                if matches!(m, Mode::Visual | Mode::VisualLine) {
                    self.sel.anchor = self.sel.head;
                }
                self.mode = m;
            }
            Action::EnterInsert(pos) => {
                self.enter_insert(pos);
            }
            Action::Operate(op, m, n) => {
                // `G` and `gg` with an operator behave linewise (vim parity).
                if matches!(m, Motion::BufferStart | Motion::BufferEnd) {
                    let cur_line = self.cursor_line();
                    let target_line = match m {
                        Motion::BufferStart => {
                            if n == 0 {
                                0
                            } else {
                                n.saturating_sub(1)
                                    .min(self.buffer.len_lines().saturating_sub(1))
                            }
                        }
                        Motion::BufferEnd => {
                            if n == 0 {
                                self.buffer.len_lines().saturating_sub(1)
                            } else {
                                n.saturating_sub(1)
                                    .min(self.buffer.len_lines().saturating_sub(1))
                            }
                        }
                        _ => unreachable!(),
                    };
                    // The trailing newline produces an extra "empty line"; clamp to
                    // the last line that actually has a newline terminator.
                    let last_real_line = if self.buffer.len_chars() > 0
                        && self.buffer.rope().char(self.buffer.len_chars() - 1) == '\n'
                    {
                        self.buffer.len_lines().saturating_sub(2)
                    } else {
                        self.buffer.len_lines().saturating_sub(1)
                    };
                    let target_line = target_line.min(last_real_line);
                    let (lo, hi) = if cur_line <= target_line {
                        (cur_line, target_line)
                    } else {
                        (target_line, cur_line)
                    };
                    let line_count = hi - lo + 1;
                    // Reposition cursor to the start of the lower line so OperateLine
                    // works from there, then dispatch as a linewise op.
                    self.sel = Selection::at(self.buffer.line_to_char(lo)).clamped(&self.buffer);
                    let start = self.buffer.line_to_char(lo);
                    let end_line_char =
                        self.buffer.line_to_char(hi) + self.buffer.line_len_chars(hi);
                    let end = if end_line_char < self.buffer.len_chars() {
                        end_line_char + 1
                    } else {
                        end_line_char
                    };
                    let entered_insert = self.apply_operator_with_kind(op, start..end, true);
                    if !entered_insert {
                        self.last_change = Some(RepeatAction::OperateLine {
                            op,
                            count: line_count,
                        });
                    }
                    return;
                }

                // `cw` / `cW` are vim-special: they act like `ce` / `cE`,
                // i.e. change to end-of-word without consuming the trailing
                // whitespace. We rewrite the motion before evaluating it.
                let m = if matches!(op, PendingOp::Change) && matches!(m, Motion::WordForward) {
                    Motion::WordEnd
                } else {
                    m
                };
                let target = apply_motion(&self.buffer, self.sel, m, n);
                let inclusive = matches!(
                    m,
                    Motion::LineEnd
                        | Motion::WordEnd
                        | Motion::FindChar(_, _, FindKind::On)
                        | Motion::MatchBracket
                );
                let range = if self.sel.head <= target.head {
                    let end = if inclusive {
                        (target.head + 1).min(self.buffer.len_chars())
                    } else {
                        target.head
                    };
                    self.sel.head..end
                } else {
                    target.head..self.sel.head
                };
                let entered_insert = self.apply_operator(op, range);
                if entered_insert {
                    // `c<motion>` — record origin so leave_insert can build a
                    // full ChangeMotion replay including the typed text.
                    if let Some(pi) = self.pending_insert.as_mut() {
                        pi.origin = InsertOrigin::ChangeMotion {
                            motion: m,
                            count: n,
                        };
                    }
                } else {
                    self.last_change = Some(RepeatAction::Operate {
                        op,
                        motion: m,
                        count: n,
                    });
                }
            }
            Action::OperateObject(op, obj, kind, _n) => {
                // `n` > 1 for text objects is uncommon and semantically fuzzy;
                // apply once for now.
                if let Some(range) = text_object_range(&self.buffer, self.sel.head, obj, kind) {
                    let entered_insert = self.apply_operator(op, range);
                    if entered_insert {
                        if let Some(pi) = self.pending_insert.as_mut() {
                            pi.origin = InsertOrigin::ChangeObject { object: obj, kind };
                        }
                    } else {
                        self.last_change = Some(RepeatAction::OperateObject {
                            op,
                            object: obj,
                            kind,
                            count: 1,
                        });
                    }
                } else {
                    self.msg = "no matching text object".into();
                }
            }
            Action::OperateLine(op, n) => {
                let n = n.max(1);
                let line = self.cursor_line();
                let start = self.buffer.line_to_char(line);
                // `dd` = 1 line = [line, line]. `2dd` = 2 lines = [line, line+1].
                let end_line = (line + n - 1).min(self.buffer.len_lines().saturating_sub(1));
                let end = self.buffer.line_to_char(end_line) + self.buffer.line_len_chars(end_line);
                // For Change (cc): keep the trailing newline so we end up on a
                // blank line in place. For everything else (dd / yy / >>):
                // include the trailing newline so the row goes away.
                let include_newline = !matches!(op, PendingOp::Change);
                let end = if include_newline && end < self.buffer.len_chars() {
                    end + 1
                } else {
                    end
                };
                let entered_insert = self.apply_operator_with_kind(op, start..end, true);
                if entered_insert {
                    if let Some(pi) = self.pending_insert.as_mut() {
                        pi.origin = InsertOrigin::ChangeLine { count: n };
                    }
                } else {
                    self.last_change = Some(RepeatAction::OperateLine { op, count: n });
                }
            }
            Action::Undo => {
                if let Some(restore) = self.history.undo(&mut self.buffer) {
                    self.sel = restore.clamped(&self.buffer);
                } else {
                    self.msg = "Already at oldest change".into();
                }
            }
            Action::Redo => {
                if let Some(restore) = self.history.redo(&mut self.buffer) {
                    self.sel = restore.clamped(&self.buffer);
                } else {
                    self.msg = "Already at newest change".into();
                }
            }
            Action::RepeatLastChange => {
                self.repeat_last_change();
            }
            Action::Paste { after, count } => {
                for _ in 0..count.max(1) {
                    self.paste(after);
                }
                self.last_change = Some(RepeatAction::Paste { after, count });
            }
            Action::ExCommand(cmd) => {
                self.run_ex(&cmd);
            }
            Action::EnterSearch(dir) => {
                self.cmdline.clear();
                self.cmdline_prompt = match dir {
                    SearchDirection::Forward => '/',
                    SearchDirection::Backward => '?',
                };
                self.mode = Mode::Command;
            }
            Action::SearchRepeat(dir) => {
                self.push_jump();
                self.search_repeat(dir);
            }
            Action::WordSearchUnder(dir) => {
                self.push_jump();
                self.word_search_under(dir);
            }
            Action::ToggleCase(n) => {
                let start = self.sel.head;
                let n = n.max(1);
                let end = (start + n).min(self.buffer.len_chars());
                if start < end {
                    self.apply_operator(PendingOp::SwapCase, start..end);
                    // `~` advances the cursor by `n` (capped at end-of-line in
                    // Normal mode, vim semantics).
                    let (line, _) = self.buffer.char_to_line_col(start);
                    let line_end = self.buffer.line_to_char(line)
                        + self.buffer.line_len_chars(line).saturating_sub(1);
                    self.sel = Selection::at(end.min(line_end)).clamped(&self.buffer);
                }
            }
            Action::DeleteChars { forward, count } => {
                let range = self.delete_chars_range(forward, count);
                if range.start < range.end {
                    self.apply_operator(PendingOp::Delete, range);
                    self.last_change = Some(RepeatAction::DeleteChars { forward, count });
                }
            }
            Action::RepeatFind { reverse, count } => {
                let Some((c, dir, kind)) = self.last_find else {
                    self.msg = "No previous f/F/t/T".into();
                    return;
                };
                let effective_dir = if reverse {
                    match dir {
                        FindDirection::Forward => FindDirection::Backward,
                        FindDirection::Backward => FindDirection::Forward,
                    }
                } else {
                    dir
                };
                self.sel = apply_motion(
                    &self.buffer,
                    self.sel,
                    Motion::FindChar(c, effective_dir, kind),
                    count,
                )
                .clamped(&self.buffer);
            }
            Action::LspHover => {
                self.request_hover();
            }
            Action::LspGotoDefinition => {
                self.request_definition();
            }
            Action::LspCodeAction => {
                self.run_code_action();
            }
            Action::JumpBack => {
                self.jump_back();
            }
            Action::JumpForward => {
                self.jump_forward();
            }
            Action::Pending | Action::Unhandled => {}
        }
    }

    /// Position cursor for the given insert style and begin recording.
    pub(crate) fn enter_insert(&mut self, pos: InsertPos) {
        let sel_before = self.sel;
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        let mut tx = Transaction::new();
        tx.sel_before = Some(sel_before);

        match pos {
            InsertPos::AtCursor => {}
            InsertPos::AfterCursor => {
                let line_len = self.buffer.line_len_chars(line);
                if col < line_len {
                    self.sel.head += 1;
                    self.sel.anchor = self.sel.head;
                }
            }
            InsertPos::BeforeLine => {
                self.sel = apply_motion(&self.buffer, self.sel, Motion::LineFirstNonBlank, 1);
            }
            InsertPos::AfterLine => {
                self.sel = apply_motion(&self.buffer, self.sel, Motion::LineEnd, 1);
                let line_len = self.buffer.line_len_chars(self.cursor_line());
                if line_len > 0 {
                    self.sel.head += 1;
                    self.sel.anchor = self.sel.head;
                }
            }
            InsertPos::OpenBelow => {
                let line_end = self.buffer.line_to_char(line) + self.buffer.line_len_chars(line);
                self.buffer.insert_char(line_end, '\n');
                tx.push(Change::Insert {
                    at: line_end,
                    text: "\n".into(),
                });
                self.sel = Selection::at(line_end + 1);
            }
            InsertPos::OpenAbove => {
                let line_start = self.buffer.line_to_char(line);
                self.buffer.insert_char(line_start, '\n');
                tx.push(Change::Insert {
                    at: line_start,
                    text: "\n".into(),
                });
                self.sel = Selection::at(line_start);
            }
        }

        self.pending_insert = Some(PendingInsert {
            pos,
            tx,
            typed: String::new(),
            origin: InsertOrigin::Plain,
        });
        self.mode = Mode::Insert;
    }

    /// Returns true if the operator entered Insert mode (c/cc).
    pub(crate) fn apply_operator(&mut self, op: PendingOp, range: std::ops::Range<usize>) -> bool {
        self.apply_operator_with_kind(op, range, false)
    }

    /// Linewise-aware operator application. `linewise` affects the register
    /// tag on yank/delete.
    pub(crate) fn apply_operator_with_kind(
        &mut self,
        op: PendingOp,
        range: std::ops::Range<usize>,
        linewise: bool,
    ) -> bool {
        match op {
            PendingOp::Delete => {
                let removed: String = self.buffer.rope().slice(range.clone()).to_string();
                self.register = Register {
                    text: removed.clone(),
                    linewise,
                };
                let sel_before = self.sel;
                self.buffer.remove_range(range.clone());
                let sel_after = Selection::at(range.start).clamped(&self.buffer);
                self.sel = sel_after;
                let mut tx = Transaction::new();
                tx.sel_before = Some(sel_before);
                tx.push(Change::Delete {
                    at: range.start,
                    removed,
                });
                tx.sel_after = Some(sel_after);
                self.history.commit(tx);
                false
            }
            PendingOp::Change => {
                let removed: String = self.buffer.rope().slice(range.clone()).to_string();
                self.register = Register {
                    text: removed.clone(),
                    linewise,
                };
                let sel_before = self.sel;
                self.buffer.remove_range(range.clone());
                let sel_after = Selection::at(range.start).clamped(&self.buffer);
                self.sel = sel_after;
                let mut tx = Transaction::new();
                tx.sel_before = Some(sel_before);
                tx.push(Change::Delete {
                    at: range.start,
                    removed,
                });
                self.pending_insert = Some(PendingInsert {
                    pos: InsertPos::AtCursor,
                    tx,
                    typed: String::new(),
                    // The caller (Operate / OperateLine / OperateObject) overwrites
                    // this with the appropriate origin so `.` replays the full
                    // change (deletion + typed text). Default to Plain in case
                    // some path forgets to set it.
                    origin: InsertOrigin::Plain,
                });
                self.mode = Mode::Insert;
                true
            }
            PendingOp::Yank => {
                let text: String = self.buffer.rope().slice(range.clone()).to_string();
                osc52_copy(&text);
                self.register = Register { text, linewise };
                self.yank_flash =
                    Some((range.clone(), Instant::now() + Duration::from_millis(150)));
                // Yank doesn't move cursor in Normal, but in Visual it returns to
                // Normal mode with cursor at start of selection (Vim quirk).
                if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
                    self.sel = Selection::at(range.start).clamped(&self.buffer);
                }
                false
            }
            PendingOp::SwapCase => {
                let source: String = self.buffer.rope().slice(range.clone()).to_string();
                let replacement: String = source
                    .chars()
                    .map(|c| {
                        if c.is_uppercase() {
                            c.to_lowercase().next().unwrap_or(c)
                        } else if c.is_lowercase() {
                            c.to_uppercase().next().unwrap_or(c)
                        } else {
                            c
                        }
                    })
                    .collect();
                self.replace_range(range, &source, &replacement);
                false
            }
            PendingOp::ToLower => {
                let source: String = self.buffer.rope().slice(range.clone()).to_string();
                let replacement = source.to_lowercase();
                self.replace_range(range, &source, &replacement);
                false
            }
            PendingOp::ToUpper => {
                let source: String = self.buffer.rope().slice(range.clone()).to_string();
                let replacement = source.to_uppercase();
                self.replace_range(range, &source, &replacement);
                false
            }
            PendingOp::ShiftRight => {
                self.indent_range(range, true);
                false
            }
            PendingOp::ShiftLeft => {
                self.indent_range(range, false);
                false
            }
            PendingOp::ToggleComment => {
                self.toggle_comment_range(range);
                false
            }
            _ => {
                self.msg = format!("{op:?} not yet implemented");
                false
            }
        }
    }

    /// Replace `range` (currently holding `old`) with `new_text` as one transaction.
    pub(crate) fn replace_range(
        &mut self,
        range: std::ops::Range<usize>,
        old: &str,
        new_text: &str,
    ) {
        let sel_before = self.sel;
        self.buffer.remove_range(range.clone());
        self.buffer.insert_str(range.start, new_text);
        let new_len = new_text.chars().count();
        self.sel = Selection::at(range.start + new_len.saturating_sub(1)).clamped(&self.buffer);
        let mut tx = Transaction::new();
        tx.sel_before = Some(sel_before);
        tx.push(Change::Delete {
            at: range.start,
            removed: old.to_string(),
        });
        tx.push(Change::Insert {
            at: range.start,
            text: new_text.to_string(),
        });
        tx.sel_after = Some(self.sel);
        self.history.commit(tx);
    }

    /// Indent or outdent the lines touched by `range`. 4 spaces per level.
    pub(crate) fn indent_range(&mut self, range: std::ops::Range<usize>, right: bool) {
        let (first_line, _) = self.buffer.char_to_line_col(range.start);
        let (mut last_line, _) = self.buffer.char_to_line_col(range.end.saturating_sub(1));
        if range.end == 0 {
            last_line = first_line;
        }
        let sel_before = self.sel;
        let mut tx = Transaction::new();
        tx.sel_before = Some(sel_before);

        // Operate bottom-up so earlier line offsets stay valid.
        for line in (first_line..=last_line).rev() {
            let start = self.buffer.line_to_char(line);
            if right {
                self.buffer.insert_str(start, "    ");
                tx.push(Change::Insert {
                    at: start,
                    text: "    ".into(),
                });
            } else {
                let line_text: String = self.buffer.rope().line(line).chars().collect();
                let to_strip = line_text.chars().take_while(|c| *c == ' ').take(4).count();
                if to_strip > 0 {
                    let removed: String = " ".repeat(to_strip);
                    self.buffer.remove_range(start..(start + to_strip));
                    tx.push(Change::Delete { at: start, removed });
                }
            }
        }
        self.sel = Selection::at(self.buffer.line_to_char(first_line)).clamped(&self.buffer);
        self.sel = apply_motion(&self.buffer, self.sel, Motion::LineFirstNonBlank, 1);
        tx.sel_after = Some(self.sel);
        self.history.commit(tx);
    }

    /// Toggle line comments across the lines touched by `range`. Comment
    /// state is determined by the non-blank lines: if every non-blank line
    /// already starts with the language's line-comment prefix, uncomment;
    /// otherwise comment by inserting at the minimum indent column.
    pub(crate) fn toggle_comment_range(&mut self, range: std::ops::Range<usize>) {
        let Some(prefix) = self
            .syntax
            .as_ref()
            .and_then(|s| s.language().line_comment())
        else {
            self.msg = "No line comment for this language".into();
            return;
        };
        let (first_line, _) = self.buffer.char_to_line_col(range.start);
        let (mut last_line, _) = self.buffer.char_to_line_col(range.end.saturating_sub(1));
        if range.end == 0 {
            last_line = first_line;
        }

        let line_text = |buf: &Buffer, line: usize| -> String {
            let raw: String = buf.rope().line(line).chars().collect();
            raw.trim_end_matches('\n').to_string()
        };

        let mut all_commented = true;
        let mut min_indent = usize::MAX;
        let mut any_non_blank = false;
        for line in first_line..=last_line {
            let text = line_text(&self.buffer, line);
            let indent = text.chars().take_while(|c| c.is_whitespace()).count();
            if indent == text.chars().count() {
                continue;
            }
            any_non_blank = true;
            if indent < min_indent {
                min_indent = indent;
            }
            let after_indent: String = text.chars().skip(indent).collect();
            if !after_indent.starts_with(prefix) {
                all_commented = false;
            }
        }
        if !any_non_blank {
            return;
        }
        if min_indent == usize::MAX {
            min_indent = 0;
        }

        let sel_before = self.sel;
        let mut tx = Transaction::new();
        tx.sel_before = Some(sel_before);

        if all_commented {
            let prefix_chars = prefix.chars().count();
            for line in (first_line..=last_line).rev() {
                let text = line_text(&self.buffer, line);
                let indent = text.chars().take_while(|c| c.is_whitespace()).count();
                if indent == text.chars().count() {
                    continue;
                }
                let after_indent: String = text.chars().skip(indent).collect();
                if !after_indent.starts_with(prefix) {
                    continue;
                }
                let line_start = self.buffer.line_to_char(line);
                let remove_start = line_start + indent;
                let trailing_space = if after_indent.chars().nth(prefix_chars) == Some(' ') {
                    1
                } else {
                    0
                };
                let remove_count = prefix_chars + trailing_space;
                let removed: String = self
                    .buffer
                    .rope()
                    .slice(remove_start..(remove_start + remove_count))
                    .to_string();
                self.buffer
                    .remove_range(remove_start..(remove_start + remove_count));
                tx.push(Change::Delete {
                    at: remove_start,
                    removed,
                });
            }
        } else {
            let to_insert = format!("{} ", prefix);
            for line in (first_line..=last_line).rev() {
                let text = line_text(&self.buffer, line);
                let indent = text.chars().take_while(|c| c.is_whitespace()).count();
                if indent == text.chars().count() {
                    continue;
                }
                let line_start = self.buffer.line_to_char(line);
                let insert_at = line_start + min_indent;
                self.buffer.insert_str(insert_at, &to_insert);
                tx.push(Change::Insert {
                    at: insert_at,
                    text: to_insert.clone(),
                });
            }
        }

        self.sel = Selection::at(self.buffer.line_to_char(first_line)).clamped(&self.buffer);
        self.sel = apply_motion(&self.buffer, self.sel, Motion::LineFirstNonBlank, 1);
        tx.sel_after = Some(self.sel);
        self.history.commit(tx);
    }

    /// Paste the unnamed register after (`p`) or before (`P`) the cursor.
    pub(crate) fn paste(&mut self, after: bool) {
        if self.register.text.is_empty() {
            self.msg = "Register empty".into();
            return;
        }
        let text = self.register.text.clone();
        let sel_before = self.sel;
        let mut tx = Transaction::new();
        tx.sel_before = Some(sel_before);

        let insert_at;
        let cursor_after;
        if self.register.linewise {
            let (line, _) = self.buffer.char_to_line_col(self.sel.head);
            insert_at = if after {
                self.buffer.line_to_char(line)
                    + self.buffer.line_len_chars(line)
                    + if line + 1 < self.buffer.len_lines() {
                        1
                    } else {
                        0
                    }
            } else {
                self.buffer.line_to_char(line)
            };
            // Ensure the pasted block ends with a newline for clean line boundary.
            let to_insert = if text.ends_with('\n') {
                text.clone()
            } else {
                format!("{text}\n")
            };
            self.buffer.insert_str(insert_at, &to_insert);
            tx.push(Change::Insert {
                at: insert_at,
                text: to_insert.clone(),
            });
            cursor_after = insert_at;
        } else {
            let (line, col) = self.buffer.char_to_line_col(self.sel.head);
            let line_len = self.buffer.line_len_chars(line);
            insert_at = if after && col < line_len {
                self.sel.head + 1
            } else {
                self.sel.head
            };
            self.buffer.insert_str(insert_at, &text);
            tx.push(Change::Insert {
                at: insert_at,
                text: text.clone(),
            });
            let n = text.chars().count();
            cursor_after = insert_at + n.saturating_sub(1);
        }

        self.sel = Selection::at(cursor_after).clamped(&self.buffer);
        tx.sel_after = Some(self.sel);
        self.history.commit(tx);
    }

    /// Re-dispatch the last change at the current cursor.
    pub(crate) fn repeat_last_change(&mut self) {
        let Some(last) = self.last_change.clone() else {
            self.msg = "No previous change to repeat".into();
            return;
        };
        match last {
            RepeatAction::Operate { op, motion, count } => {
                let target = apply_motion(&self.buffer, self.sel, motion, count);
                let range = if self.sel.head <= target.head {
                    self.sel.head..target.head
                } else {
                    target.head..self.sel.head
                };
                self.apply_operator(op, range);
            }
            RepeatAction::OperateLine { op, count } => {
                let count = count.max(1);
                let line = self.cursor_line();
                let start = self.buffer.line_to_char(line);
                let end_line = (line + count - 1).min(self.buffer.len_lines().saturating_sub(1));
                let end = self.buffer.line_to_char(end_line) + self.buffer.line_len_chars(end_line);
                let end = if end < self.buffer.len_chars() {
                    end + 1
                } else {
                    end
                };
                self.apply_operator_with_kind(op, start..end, true);
            }
            RepeatAction::OperateObject {
                op,
                object,
                kind,
                count: _,
            } => {
                if let Some(range) = text_object_range(&self.buffer, self.sel.head, object, kind) {
                    self.apply_operator(op, range);
                }
            }
            RepeatAction::InsertBurst { pos, text } => {
                self.enter_insert(pos);
                for c in text.chars() {
                    self.insert_char_in_session(c);
                }
                self.leave_insert();
            }
            RepeatAction::DeleteChars { forward, count } => {
                let range = self.delete_chars_range(forward, count);
                if range.start < range.end {
                    self.apply_operator(PendingOp::Delete, range);
                }
            }
            RepeatAction::ChangeMotion {
                motion,
                count,
                text,
            } => {
                // Re-evaluate the motion at the current cursor, delete that
                // range as a Change (which enters insert), then synthesize the
                // typed text and leave insert. The whole thing collapses into
                // one history transaction via leave_insert.
                let m = if matches!(motion, Motion::WordForward) {
                    Motion::WordEnd
                } else {
                    motion
                };
                let target = apply_motion(&self.buffer, self.sel, m, count);
                let inclusive = matches!(
                    m,
                    Motion::LineEnd
                        | Motion::WordEnd
                        | Motion::FindChar(_, _, FindKind::On)
                        | Motion::MatchBracket
                );
                let range = if self.sel.head <= target.head {
                    let end = if inclusive {
                        (target.head + 1).min(self.buffer.len_chars())
                    } else {
                        target.head
                    };
                    self.sel.head..end
                } else {
                    target.head..self.sel.head
                };
                self.apply_operator(PendingOp::Change, range);
                for c in text.chars() {
                    self.insert_char_in_session(c);
                }
                self.leave_insert();
            }
            RepeatAction::ChangeObject { object, kind, text } => {
                if let Some(range) = text_object_range(&self.buffer, self.sel.head, object, kind) {
                    self.apply_operator(PendingOp::Change, range);
                    for c in text.chars() {
                        self.insert_char_in_session(c);
                    }
                    self.leave_insert();
                }
            }
            RepeatAction::ChangeLine { count, text } => {
                let count = count.max(1);
                let line = self.cursor_line();
                let start = self.buffer.line_to_char(line);
                let end_line = (line + count - 1).min(self.buffer.len_lines().saturating_sub(1));
                let end = self.buffer.line_to_char(end_line) + self.buffer.line_len_chars(end_line);
                self.apply_operator_with_kind(PendingOp::Change, start..end, true);
                for c in text.chars() {
                    self.insert_char_in_session(c);
                }
                self.leave_insert();
            }
            RepeatAction::Paste { after, count } => {
                for _ in 0..count.max(1) {
                    self.paste(after);
                }
            }
        }
    }

    /// Compute the char range for `x` / `X`: `count` chars forward or backward
    /// from the cursor, clamped to the current line (so x on the last char of
    /// a line deletes that char, and X at col 0 is a no-op).
    pub(crate) fn delete_chars_range(&self, forward: bool, count: usize) -> std::ops::Range<usize> {
        let count = count.max(1);
        let head = self.sel.head;
        let (line, _) = self.buffer.char_to_line_col(head);
        let line_start = self.buffer.line_to_char(line);
        let line_end = line_start + self.buffer.line_len_chars(line);
        if forward {
            head..(head + count).min(line_end)
        } else {
            head.saturating_sub(count).max(line_start)..head
        }
    }

    /// Insert a single character within the current Insert session. Records it
    /// into the pending transaction and the dot-repeat buffer.
    pub(crate) fn insert_char_in_session(&mut self, c: char) {
        let at = self.sel.head;
        self.buffer.insert_char(at, c);
        self.sel.head += 1;
        self.sel.anchor = self.sel.head;
        if let Some(pi) = self.pending_insert.as_mut() {
            let text = c.to_string();
            pi.tx.push(Change::Insert {
                at,
                text: text.clone(),
            });
            pi.typed.push(c);
        }
    }

    /// Backspace in Insert mode. Records the deletion.
    pub(crate) fn backspace_in_session(&mut self) {
        if self.sel.head == 0 {
            return;
        }
        let at = self.sel.head - 1;
        let removed: String = self.buffer.rope().char(at).to_string();
        self.buffer.remove_range(at..self.sel.head);
        self.sel.head = at;
        self.sel.anchor = self.sel.head;
        if let Some(pi) = self.pending_insert.as_mut() {
            pi.tx.push(Change::Delete { at, removed });
            pi.typed.pop();
        }
    }

    /// Commit the current Insert session to history and record it for `.`.
    pub(crate) fn leave_insert(&mut self) {
        if let Some(mut pi) = self.pending_insert.take() {
            pi.tx.sel_after = Some(self.sel);
            self.history.commit(pi.tx);
            self.last_change = Some(match pi.origin {
                InsertOrigin::Plain => RepeatAction::InsertBurst {
                    pos: pi.pos,
                    text: pi.typed,
                },
                InsertOrigin::ChangeMotion { motion, count } => RepeatAction::ChangeMotion {
                    motion,
                    count,
                    text: pi.typed,
                },
                InsertOrigin::ChangeObject { object, kind } => RepeatAction::ChangeObject {
                    object,
                    kind,
                    text: pi.typed,
                },
                InsertOrigin::ChangeLine { count } => RepeatAction::ChangeLine {
                    count,
                    text: pi.typed,
                },
            });
        }
        self.mode = Mode::Normal;
        // Vim moves cursor left on leaving insert (unless at line start).
        let (_, col) = self.buffer.char_to_line_col(self.sel.head);
        if col > 0 {
            self.sel.head -= 1;
            self.sel.anchor = self.sel.head;
        }
    }
}
