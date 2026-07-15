//! Jump-list navigation: `Ctrl-O` / `Ctrl-I` back/forward through recorded
//! cursor positions.

use vix_core::{Buffer, JumpEntry, Selection};

use crate::Editor;

impl Editor {
    /// Capture the current cursor position as a jump-list entry.
    pub(crate) fn current_jump_entry(&self) -> JumpEntry {
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        JumpEntry {
            path: self.buffer.path().map(|p| p.to_path_buf()),
            line,
            col,
        }
    }

    /// Push the *current* cursor position onto the jump list. Call this
    /// immediately before a "big jump" action — buffer switch, gg/G, search,
    /// goto-definition, etc.
    pub(crate) fn push_jump(&mut self) {
        self.jumps.push(self.current_jump_entry());
    }

    /// Move the active buffer + cursor to the entry. If the target lives in a
    /// different buffer (or an on-disk file not currently open), we switch or
    /// load it. Returns false if the buffer couldn't be located or loaded.
    pub(crate) fn goto_jump_entry(&mut self, entry: JumpEntry) -> bool {
        let same_buffer = match (&entry.path, self.buffer.path()) {
            (Some(p), Some(cur)) => p.as_path() == cur,
            (None, None) => true,
            _ => false,
        };
        if !same_buffer {
            if let Some(path) = entry.path.as_ref() {
                if let Some(idx) = self.buffer_index_by_path(path) {
                    if idx != 0 {
                        self.switch_to_buffer(idx);
                    }
                } else if path.exists() {
                    match Buffer::load(path) {
                        Ok(buf) => self.add_or_switch_buffer(buf),
                        Err(e) => {
                            self.msg = format!("jump: {e}");
                            return false;
                        }
                    }
                } else {
                    self.msg = format!("jump: {} is gone", path.display());
                    return false;
                }
            } else {
                // entry points at the unnamed buffer but we're somewhere else
                self.msg = "jump: origin buffer is gone".into();
                return false;
            }
        }
        let line = entry.line.min(self.buffer.len_lines().saturating_sub(1));
        let line_start = self.buffer.line_to_char(line);
        let col = entry.col.min(self.buffer.line_len_chars(line));
        self.sel = Selection::at(line_start + col).clamped(&self.buffer);
        true
    }

    /// `Ctrl-O` — step back through the jump list.
    pub(crate) fn jump_back(&mut self) {
        let current = self.current_jump_entry();
        match self.jumps.back(current) {
            Some(e) => {
                if !self.goto_jump_entry(e) { /* msg set in callee */ }
            }
            None => self.msg = "at top of jump list".into(),
        }
    }

    /// `Ctrl-I` / Tab — step forward.
    pub(crate) fn jump_forward(&mut self) {
        match self.jumps.forward() {
            Some(e) => {
                if !self.goto_jump_entry(e) { /* msg set in callee */ }
            }
            None => self.msg = "at bottom of jump list".into(),
        }
    }
}
