//! Input handling: translating raw key/mouse events into editor actions.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use vix_core::{
    apply_motion, handle_normal_char, text_object_range, Action, Mode, Motion, NormalKeyState,
    PendingOp, SearchDirection, Selection, TextObject, TextObjectKind,
};

use crate::Editor;

impl Editor {
    /// Handle a single key event for the current mode.
    pub fn handle_key(&mut self, k: KeyEvent) {
        self.msg.clear();
        // Any keypress closes the hover popup (except when the picker is up).
        if self.picker.is_none() {
            self.hover_popup = None;
        }

        // Picker intercepts input while it's up.
        if self.picker.is_some() {
            self.handle_picker_key(k);
            return;
        }

        // Global: Ctrl-C always returns to Normal (escape hatch).
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            if self.mode == Mode::Insert {
                self.leave_insert();
            }
            self.mode = Mode::Normal;
            self.keys = NormalKeyState::default();
            self.cmdline.clear();
            self.pending_insert = None;
            self.pending_leader = false;
            return;
        }

        // Ctrl-R in Normal mode = redo.
        if self.mode == Mode::Normal
            && k.modifiers.contains(KeyModifiers::CONTROL)
            && k.code == KeyCode::Char('r')
        {
            self.dispatch(Action::Redo);
            return;
        }

        // Ctrl-O / Ctrl-I in Normal mode: jump list back / forward.
        if self.mode == Mode::Normal
            && k.modifiers.contains(KeyModifiers::CONTROL)
            && k.code == KeyCode::Char('o')
        {
            self.jump_back();
            return;
        }
        if self.mode == Mode::Normal
            && k.modifiers.contains(KeyModifiers::CONTROL)
            && k.code == KeyCode::Char('i')
        {
            self.jump_forward();
            return;
        }

        // Ctrl-P in Normal mode: open the omnibox (file names + contents).
        // The picker intercepts keys first while it's up, so its own Ctrl-P
        // (selection up) still wins once open; Insert-mode Ctrl-P completion
        // lives in the Insert arm and is untouched.
        if self.mode == Mode::Normal
            && k.modifiers.contains(KeyModifiers::CONTROL)
            && k.code == KeyCode::Char('p')
        {
            self.open_files_picker();
            return;
        }

        // Tab / Shift-Tab in Normal mode: cycle to next/prev open buffer
        // (Alt-Tab style — wraps around at the ends).
        if self.mode == Mode::Normal && k.code == KeyCode::Tab {
            if k.modifiers.contains(KeyModifiers::SHIFT) {
                self.prev_buffer();
            } else {
                self.next_buffer();
            }
            return;
        }
        if self.mode == Mode::Normal && k.code == KeyCode::BackTab {
            self.prev_buffer();
            return;
        }

        // Arrow keys mirror hjkl. In Normal/Visual they go through the keymap
        // so they compose with operators (`d<Right>` works like `dl`); in
        // Insert they move the cursor one step. The completion popup keeps
        // first dibs on Up/Down when it's open.
        let arrow_char = match k.code {
            KeyCode::Left => Some('h'),
            KeyCode::Right => Some('l'),
            KeyCode::Up => Some('k'),
            KeyCode::Down => Some('j'),
            _ => None,
        };
        if let Some(c) = arrow_char {
            let popup_handles = self.mode == Mode::Insert
                && self.completion_popup.is_some()
                && matches!(k.code, KeyCode::Up | KeyCode::Down);
            if !popup_handles {
                match self.mode {
                    Mode::Normal | Mode::Visual | Mode::VisualLine => {
                        let action = handle_normal_char(&mut self.keys, c);
                        self.dispatch(action);
                        return;
                    }
                    Mode::Insert => {
                        let motion = match c {
                            'h' => Motion::Left,
                            'l' => Motion::Right,
                            'k' => Motion::Up,
                            'j' => Motion::Down,
                            _ => unreachable!(),
                        };
                        // Cursor move closes any completion popup and breaks
                        // the insert session for `.`-repeat parity.
                        self.completion_popup = None;
                        self.sel =
                            apply_motion(&self.buffer, self.sel, motion, 1).clamped(&self.buffer);
                        if let Some(pi) = self.pending_insert.as_mut() {
                            pi.typed.clear();
                        }
                        return;
                    }
                    Mode::Command => return,
                }
            }
        }

        // PageUp / PageDown: jump the cursor by ~one screen. `ensure_cursor_visible`
        // at render time pulls `view_top` along, the same way mouse-wheel scroll
        // works. No vim keymap entry, so we move the cursor directly.
        if matches!(k.code, KeyCode::PageUp | KeyCode::PageDown) {
            if self.mode == Mode::Command {
                return;
            }
            let step = self.page_step();
            let motion = if k.code == KeyCode::PageUp {
                Motion::Up
            } else {
                Motion::Down
            };
            self.completion_popup = None;
            let new_sel = apply_motion(&self.buffer, self.sel, motion, step);
            self.sel = if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
                // Extend: keep anchor, move head only — same as Action::Move.
                Selection {
                    anchor: self.sel.anchor,
                    head: new_sel.head,
                    virt_col: new_sel.virt_col,
                }
            } else {
                new_sel
            }
            .clamped(&self.buffer);
            if self.mode == Mode::Insert {
                if let Some(pi) = self.pending_insert.as_mut() {
                    pi.typed.clear();
                }
            }
            return;
        }

        match self.mode {
            Mode::Normal => {
                if let KeyCode::Char(c) = k.code {
                    let unmodified = !k.modifiers.contains(KeyModifiers::CONTROL)
                        && !k.modifiers.contains(KeyModifiers::ALT);
                    if unmodified {
                        if self.pending_leader {
                            self.pending_leader = false;
                            match c {
                                'f' => {
                                    self.open_files_picker();
                                    return;
                                }
                                'g' => {
                                    self.open_grep_picker("");
                                    return;
                                }
                                'b' => {
                                    self.open_buffers_picker();
                                    return;
                                }
                                _ => return,
                            }
                        }
                        if c == ' ' {
                            self.pending_leader = true;
                            return;
                        }
                    }
                    let action = handle_normal_char(&mut self.keys, c);
                    self.dispatch(action);
                } else if k.code == KeyCode::Esc {
                    self.keys = NormalKeyState::default();
                    self.pending_leader = false;
                }
            }
            Mode::Visual | Mode::VisualLine => {
                if k.code == KeyCode::Esc {
                    self.keys = NormalKeyState::default();
                    self.visual_object_kind = None;
                    self.mode = Mode::Normal;
                    self.sel.anchor = self.sel.head;
                } else if let KeyCode::Char(c) = k.code {
                    // Text-object selection inside visual: `iw`, `a"`, etc.
                    // First key (`i` / `a`) sets the kind; second key picks
                    // the object and we extend the visual selection to cover
                    // the object's range.
                    if let Some(kind) = self.visual_object_kind.take() {
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
                        if let Some(o) = obj {
                            if let Some(range) =
                                text_object_range(&self.buffer, self.sel.head, o, kind)
                            {
                                // Replace the visual selection with the object's
                                // range, head positioned at the last included char.
                                self.sel.anchor = range.start;
                                self.sel.head = range.end.saturating_sub(1).max(range.start);
                                self.sel.virt_col = None;
                                self.sel = self.sel.clamped(&self.buffer);
                            }
                        }
                        return;
                    }
                    if c == 'i' {
                        self.visual_object_kind = Some(TextObjectKind::Inner);
                        return;
                    }
                    if c == 'a' {
                        self.visual_object_kind = Some(TextObjectKind::Around);
                        return;
                    }
                    // `gc`: toggle line comments on the visual selection. The
                    // first `g` keystroke routes through handle_normal_char and
                    // sets keys.prefix; we intercept the follow-up `c` here so
                    // it fires on the selection rather than waiting for a motion.
                    if self.keys.awaiting_g() && c == 'c' {
                        let range = self.visual_range();
                        let linewise = self.mode == Mode::VisualLine;
                        self.keys = NormalKeyState::default();
                        self.apply_operator_with_kind(PendingOp::ToggleComment, range, linewise);
                        self.mode = Mode::Normal;
                        self.sel.anchor = self.sel.head;
                        return;
                    }
                    // Operator on selection: apply immediately, leave Visual.
                    let op = match c {
                        'd' | 'x' => Some(PendingOp::Delete),
                        'c' | 's' => Some(PendingOp::Change),
                        'y' => Some(PendingOp::Yank),
                        '~' => Some(PendingOp::SwapCase),
                        _ => None,
                    };
                    if let Some(op) = op {
                        let range = self.visual_range();
                        let linewise = self.mode == Mode::VisualLine;
                        let entered_insert = self.apply_operator_with_kind(op, range, linewise);
                        if !entered_insert {
                            self.mode = Mode::Normal;
                        }
                        self.sel.anchor = self.sel.head;
                        return;
                    }
                    // `p`/`P` paste over visual selection: delete first, then paste.
                    // Vim quirk: the deleted text MUST NOT replace the unnamed
                    // register here, otherwise `p` would paste the just-deleted
                    // visual selection instead of the prior yank.
                    if c == 'p' || c == 'P' {
                        let range = self.visual_range();
                        let linewise = self.mode == Mode::VisualLine;
                        let saved = self.register.clone();
                        self.apply_operator_with_kind(PendingOp::Delete, range, linewise);
                        self.register = saved;
                        self.paste(true);
                        self.mode = Mode::Normal;
                        self.sel.anchor = self.sel.head;
                        return;
                    }
                    // Toggle between linewise and charwise.
                    if c == 'v' && self.mode == Mode::VisualLine {
                        self.mode = Mode::Visual;
                        return;
                    }
                    if c == 'V' && self.mode == Mode::Visual {
                        self.mode = Mode::VisualLine;
                        return;
                    }
                    if c == 'v' && self.mode == Mode::Visual {
                        self.mode = Mode::Normal;
                        self.sel.anchor = self.sel.head;
                        return;
                    }
                    if c == 'V' && self.mode == Mode::VisualLine {
                        self.mode = Mode::Normal;
                        self.sel.anchor = self.sel.head;
                        return;
                    }
                    // Otherwise: a motion or text-object; extend selection.
                    let action = handle_normal_char(&mut self.keys, c);
                    self.dispatch(action);
                }
            }
            Mode::Insert => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                let popup_open = self.completion_popup.is_some();

                // Ctrl-Space: primary trigger (doesn't conflict with zellij's
                // default Ctrl-N). Ctrl-N / Ctrl-P kept as fallback for users
                // outside zellij.
                let is_trigger =
                    (ctrl && matches!(k.code, KeyCode::Char(' '))) || k.code == KeyCode::Char('\0'); // some terminals send NUL for Ctrl-Space
                if is_trigger {
                    if popup_open {
                        self.move_completion_selection(1);
                    } else {
                        self.request_completion();
                    }
                    return;
                }
                if ctrl && matches!(k.code, KeyCode::Char('n')) {
                    if popup_open {
                        self.move_completion_selection(1);
                    } else {
                        self.request_completion();
                    }
                    return;
                }
                if ctrl && matches!(k.code, KeyCode::Char('p')) {
                    if popup_open {
                        self.move_completion_selection(-1);
                    } else {
                        self.request_completion();
                    }
                    return;
                }

                // Popup-only navigation / accept.
                if popup_open {
                    match k.code {
                        KeyCode::Down => {
                            self.move_completion_selection(1);
                            return;
                        }
                        KeyCode::Up => {
                            self.move_completion_selection(-1);
                            return;
                        }
                        KeyCode::Tab | KeyCode::Enter => {
                            self.accept_completion();
                            return;
                        }
                        KeyCode::Esc => {
                            self.completion_popup = None;
                            return;
                        }
                        _ => {}
                    }
                }

                match k.code {
                    KeyCode::Esc => {
                        self.leave_insert();
                    }
                    KeyCode::Char(c) => {
                        self.insert_char_in_session(c);
                        if self.completion_popup.is_some() {
                            self.refilter_completions();
                        }
                    }
                    KeyCode::Enter => {
                        self.insert_char_in_session('\n');
                    }
                    KeyCode::Backspace => {
                        self.backspace_in_session();
                        if self.completion_popup.is_some() {
                            self.refilter_completions();
                        }
                    }
                    KeyCode::Tab => {
                        for _ in 0..4 {
                            self.insert_char_in_session(' ');
                        }
                    }
                    _ => {}
                }
            }
            Mode::Command => match k.code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.cmdline.clear();
                    self.cmdline_prompt = ':';
                }
                KeyCode::Enter => {
                    let cmd = std::mem::take(&mut self.cmdline);
                    let prompt = self.cmdline_prompt;
                    self.mode = Mode::Normal;
                    self.cmdline_prompt = ':';
                    match prompt {
                        '/' => {
                            self.push_jump();
                            if let Some(line) = self.do_search(&cmd, SearchDirection::Forward) {
                                // Pin the first hit to the top of the
                                // viewport so it's easy to skim the
                                // following matches with `n`.
                                self.view_top = line;
                            }
                        }
                        '?' => {
                            self.push_jump();
                            if let Some(line) = self.do_search(&cmd, SearchDirection::Backward) {
                                self.view_top = line;
                            }
                        }
                        _ => self.run_ex(&cmd),
                    }
                }
                KeyCode::Backspace => {
                    if self.cmdline.pop().is_none() {
                        self.mode = Mode::Normal;
                        self.cmdline_prompt = ':';
                    }
                }
                KeyCode::Char(c) => {
                    self.cmdline.push(c);
                }
                _ => {}
            },
        }
    }

    /// Handle a mouse event. Left-click positions the cursor; scroll is left
    /// to the terminal's translation (which crossterm forwards as
    /// ScrollUp/Down events — caller may ignore them since the
    /// `ensure_cursor_visible` pass keeps the view aligned with the cursor).
    pub fn handle_mouse(&mut self, me: MouseEvent) {
        // Picker overlay owns the mouse while up: scroll cycles the
        // selection, left-click activates an entry.
        if self.picker.is_some() {
            self.handle_picker_mouse(me);
            return;
        }
        if self.completion_popup.is_some() {
            return;
        }
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(ch) = self.click_to_char(me.column, me.row) {
                    self.push_jump();
                    if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
                        // Extend the selection: keep anchor, move head.
                        self.sel.head = ch;
                        self.sel.virt_col = None;
                        self.sel = self.sel.clamped(&self.buffer);
                    } else {
                        self.sel = Selection::at(ch).clamped(&self.buffer);
                    }
                    self.hover_popup = None;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Drag enters/extends a charwise visual selection from the
                // initial click point. The first Down already set anchor=head;
                // here we move head only.
                if let Some(ch) = self.click_to_char(me.column, me.row) {
                    if !matches!(self.mode, Mode::Visual | Mode::VisualLine) {
                        self.mode = Mode::Visual;
                    }
                    self.sel.head = ch;
                    self.sel.virt_col = None;
                    self.sel = self.sel.clamped(&self.buffer);
                }
            }
            // Move the cursor along with the scroll so the view actually
            // advances — `ensure_cursor_visible` in render would otherwise yank
            // `view_top` back to the cursor and the scroll would feel like a
            // no-op once the cursor was on-screen.
            MouseEventKind::ScrollUp => {
                self.sel =
                    apply_motion(&self.buffer, self.sel, Motion::Up, 1).clamped(&self.buffer);
            }
            MouseEventKind::ScrollDown => {
                self.sel =
                    apply_motion(&self.buffer, self.sel, Motion::Down, 1).clamped(&self.buffer);
            }
            _ => {}
        }
    }

    /// Translate absolute terminal (col, row) to a buffer char offset, or
    /// None if the click was outside the content area / past EOF.
    pub(crate) fn click_to_char(&self, col: u16, row: u16) -> Option<usize> {
        let rect = self.last_content_rect?;
        if col < rect.x || row < rect.y || col >= rect.x + rect.width || row >= rect.y + rect.height
        {
            return None;
        }
        let screen_row = row - rect.y;
        let screen_col = col.saturating_sub(rect.x);
        // Clicks in the gutter snap to col 0 of that line.
        let in_text_col = screen_col.saturating_sub(self.last_gutter_cols) as usize;
        let line_idx = self.view_top + screen_row as usize;
        let total = self.buffer.len_lines();
        if line_idx >= total {
            return None;
        }
        let line_len = self.buffer.line_len_chars(line_idx);
        let col_clamped = in_text_col.min(line_len);
        Some(self.buffer.line_to_char(line_idx) + col_clamped)
    }
}
