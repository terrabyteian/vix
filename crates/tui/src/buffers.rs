//! Buffer management: loading, saving, parking (switching between open
//! buffers), and the per-buffer snapshot (`BufferSave`) used to park an
//! inactive buffer while another is active.

use vix_core::{Buffer, History, RepeatAction, Selection};
use vix_syntax::{HlSpan, Language, SyntaxState};

use crate::{Editor, PendingInsert};

/// Snapshot of everything per-buffer: used to park inactive buffers while
/// another is active. Switching buffers swaps one of these with the fields
/// living directly on `Editor`.
pub(crate) struct BufferSave {
    pub(crate) buffer: Buffer,
    pub(crate) sel: Selection,
    pub(crate) history: History,
    pub(crate) view_top: usize,
    pub(crate) syntax: Option<SyntaxState>,
    pub(crate) syntax_cache: Vec<HlSpan>,
    pub(crate) syntax_version: Option<u64>,
    pub(crate) pending_insert: Option<PendingInsert>,
    pub(crate) last_change: Option<RepeatAction>,
    /// Stable creation-order id. Survives swaps; used to render a steady
    /// position counter in the statusline as the user cycles buffers.
    pub(crate) bid: u64,
}

impl Editor {
    /// `:e` / `:e!` — reload the active buffer from disk. Refuses if there
    /// are unsaved changes unless `force` is true. Cursor is preserved by
    /// char-offset (clamped to the new length); history and pending state
    /// are reset since the buffer is now a fresh read from disk.
    pub(crate) fn reload_buffer(&mut self, force: bool) {
        let Some(path) = self.buffer.path().map(|p| p.to_path_buf()) else {
            self.msg = "E32: No file name".into();
            return;
        };
        if self.buffer.is_scratch() {
            self.msg = "E382: scratch buffer cannot be reloaded".into();
            return;
        }
        if !force && self.buffer.dirty() {
            self.msg = "E37: No write since last change (add ! to override)".into();
            return;
        }
        let new_buf = match Buffer::load(&path) {
            Ok(b) => b,
            Err(e) => {
                self.msg = format!("error: {e}");
                return;
            }
        };
        let prev_head = self.sel.head;
        self.buffer = new_buf;
        self.history = History::new();
        self.pending_insert = None;
        self.last_change = None;
        let len = self.buffer.len_chars();
        self.sel = Selection::at(prev_head.min(len)).clamped(&self.buffer);
        self.view_top = 0;
        self.syntax = self
            .buffer
            .path()
            .and_then(Language::from_path)
            .and_then(|l| SyntaxState::new(l).ok());
        self.invalidate_syntax_cache();
        // Tell LSP the document is gone so a fresh `didOpen` (via
        // `ensure_lsp_open`) re-syncs server state with the on-disk content.
        if let Some(doc) = self.lsp_docs.remove(&path) {
            if let Some(client) = self.lsp_clients.get(&doc.server_cmd) {
                client.did_close(doc.uri);
            }
        }
        self.diagnostics.remove(&path);
        self.ensure_lsp_open();
        self.msg = format!("\"{}\" reloaded", path.display());
    }

    pub(crate) fn open_path(&mut self, path: &std::path::Path) {
        // Record departure on any switch / load — but not when the target is
        // already the active buffer.
        let same_as_active = self.buffer.path().map(|p| p == path).unwrap_or(false);
        if !same_as_active {
            self.push_jump();
        }
        if let Some(idx) = self.buffer_index_by_path(path) {
            self.switch_to_buffer(idx);
            self.msg = format!("switched to \"{}\"", path.display());
            return;
        }
        match Buffer::load(path) {
            Ok(buf) => {
                self.add_or_switch_buffer(buf);
                self.msg = format!(
                    "opened \"{}\"",
                    self.buffer
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                );
            }
            Err(e) => self.msg = format!("error: {e}"),
        }
    }

    /// Invalidate the syntax cache. Call this after swapping the buffer so
    /// the version-compare heuristic doesn't stick on a stale parse (the new
    /// buffer starts its counter at 0).
    pub(crate) fn invalidate_syntax_cache(&mut self) {
        self.syntax_version = None;
        self.syntax_cache.clear();
    }

    /// Snapshot the currently-active buffer for parking. Leaves placeholder
    /// defaults behind (caller is expected to immediately install a new
    /// active buffer on top).
    pub(crate) fn save_active(&mut self) -> BufferSave {
        BufferSave {
            buffer: std::mem::replace(&mut self.buffer, Buffer::empty()),
            sel: std::mem::replace(&mut self.sel, Selection::at(0)),
            history: std::mem::replace(&mut self.history, History::new()),
            view_top: std::mem::replace(&mut self.view_top, 0),
            syntax: self.syntax.take(),
            syntax_cache: std::mem::take(&mut self.syntax_cache),
            syntax_version: self.syntax_version.take(),
            pending_insert: self.pending_insert.take(),
            last_change: self.last_change.take(),
            bid: self.active_bid,
        }
    }

    /// Install a saved buffer as the active one. Caller is responsible for
    /// having already saved any existing active buffer they want to preserve.
    pub(crate) fn install_active(&mut self, save: BufferSave) {
        self.buffer = save.buffer;
        self.sel = save.sel;
        self.history = save.history;
        self.view_top = save.view_top;
        self.syntax = save.syntax;
        self.syntax_cache = save.syntax_cache;
        self.syntax_version = save.syntax_version;
        self.pending_insert = save.pending_insert;
        self.last_change = save.last_change;
        self.active_bid = save.bid;
    }

    /// Allocate a fresh buffer id and assign it to the (newly-installed)
    /// active buffer. Call after `add_or_switch_buffer` puts a brand-new
    /// buffer into the active slot.
    pub(crate) fn assign_new_active_bid(&mut self) {
        self.active_bid = self.next_bid;
        self.next_bid = self.next_bid.wrapping_add(1).max(1);
    }

    /// Replace the active buffer with a freshly-loaded one. Parks the
    /// currently-active buffer in `other_buffers` so the user can return to
    /// it. If a buffer with the same path is already open, switch to it
    /// rather than loading twice.
    pub(crate) fn add_or_switch_buffer(&mut self, buffer: Buffer) {
        if let Some(new_path) = buffer.path() {
            if let Some(idx) = self.buffer_index_by_path(new_path) {
                self.switch_to_buffer(idx);
                self.discard_active_on_swap = false;
                return;
            }
        }
        // Launch-mode placeholder consumption: if the active buffer is the
        // pristine empty buffer we created at startup, drop it instead of
        // parking so the user's first selection is their first buffer.
        let drop_placeholder = self.discard_active_on_swap
            && self.buffer.path().is_none()
            && !self.buffer.dirty()
            && self.buffer.rope().len_chars() == 0;
        self.discard_active_on_swap = false;
        if !drop_placeholder {
            let current = self.save_active();
            self.other_buffers.push(current);
        }
        self.buffer = buffer;
        self.sel = Selection::at(0);
        self.history = History::new();
        self.view_top = 0;
        self.pending_insert = None;
        self.last_change = None;
        self.assign_new_active_bid();
        self.syntax = self
            .buffer
            .path()
            .and_then(Language::from_path)
            .and_then(|l| SyntaxState::new(l).ok());
        self.invalidate_syntax_cache();
        self.ensure_lsp_open();
    }

    /// Find a buffer by path. Index 0 = active; 1..N = `other_buffers[i-1]`.
    pub(crate) fn buffer_index_by_path(&self, path: &std::path::Path) -> Option<usize> {
        if self.buffer.path() == Some(path) {
            return Some(0);
        }
        self.other_buffers
            .iter()
            .position(|b| b.buffer.path() == Some(path))
            .map(|i| i + 1)
    }

    /// Switch the active buffer to index `idx` (0 = already active, else park
    /// current and promote `other_buffers[idx-1]`).
    pub(crate) fn switch_to_buffer(&mut self, idx: usize) {
        if idx == 0 || idx >= self.buffer_count() {
            return;
        }
        let promoted = self.other_buffers.remove(idx - 1);
        let current = self.save_active();
        self.install_active(promoted);
        self.other_buffers.push(current);
        // Buffers loaded via `load_into_park` have no syntax state and no
        // `didOpen` yet — that work was deferred so a batch open didn't
        // pay it N times. Run it now that this buffer is the active one;
        // both calls are no-ops if the previous active already had them.
        if self.syntax.is_none() {
            self.syntax = self
                .buffer
                .path()
                .and_then(Language::from_path)
                .and_then(|l| SyntaxState::new(l).ok());
            self.invalidate_syntax_cache();
        }
        self.ensure_lsp_open();
    }

    /// Load `path` and push it directly into `other_buffers` without parking
    /// the current active buffer or running syntax/LSP setup. Used by batch
    /// open (multi-select picker) so the N-1 buffers that get parked
    /// immediately don't pay for tree-sitter highlight construction or
    /// `didOpen` round trips. `initial_line` (1-based) seeds the cursor for
    /// grep-hit batch opens; pass `None` to leave it at offset 0.
    pub(crate) fn load_into_park(&mut self, path: &std::path::Path, initial_line: Option<u64>) {
        if self.buffer_index_by_path(path).is_some() {
            return;
        }
        let buf = match Buffer::load(path) {
            Ok(b) => b,
            Err(e) => {
                self.msg = format!("error: {e}");
                return;
            }
        };
        let sel = match initial_line {
            Some(line) => {
                let target =
                    (line.saturating_sub(1) as usize).min(buf.len_lines().saturating_sub(1));
                Selection::at(buf.line_to_char(target)).clamped(&buf)
            }
            None => Selection::at(0),
        };
        let bid = self.next_bid;
        self.next_bid = self.next_bid.wrapping_add(1).max(1);
        self.other_buffers.push(BufferSave {
            buffer: buf,
            sel,
            history: History::new(),
            view_top: 0,
            syntax: None,
            syntax_cache: Vec::new(),
            syntax_version: None,
            pending_insert: None,
            last_change: None,
            bid,
        });
    }

    /// `:bn` — cycle to the next buffer (wraps). No-op with a single buffer.
    pub(crate) fn next_buffer(&mut self) {
        if self.other_buffers.is_empty() {
            self.msg = "E86: Only one buffer".into();
            return;
        }
        self.push_jump();
        // The "next" buffer is conceptually the oldest parked one (FIFO).
        self.switch_to_buffer(1);
    }

    /// `:bp` — cycle to the previous buffer. Symmetric to `:bn`.
    pub(crate) fn prev_buffer(&mut self) {
        if self.other_buffers.is_empty() {
            self.msg = "E86: Only one buffer".into();
            return;
        }
        self.push_jump();
        let last = self.other_buffers.len();
        self.switch_to_buffer(last);
    }

    /// `:bd` — close the active buffer. Refuses if dirty (use `:bd!`). If
    /// there are no other buffers left, quits the editor.
    pub(crate) fn close_buffer(&mut self, force: bool) {
        if !force && self.buffer.dirty() {
            self.msg = "E89: No write since last change (use :bd!)".into();
            return;
        }
        if let Some(next) = self.other_buffers.pop() {
            self.install_active(next);
        } else {
            self.quit = true;
        }
    }

    /// True if any buffer (active or parked) has unsaved edits.
    pub(crate) fn any_buffer_dirty(&self) -> bool {
        self.buffer.dirty() || self.other_buffers.iter().any(|b| b.buffer.dirty())
    }

    /// `:b <spec>` — switch to buffer matching `spec`. Numeric = 1-based
    /// index; non-numeric = substring match over buffer paths.
    pub(crate) fn switch_buffer_by_spec(&mut self, spec: &str) {
        if let Ok(n) = spec.parse::<usize>() {
            if n == 0 || n > self.buffer_count() {
                self.msg = format!("E86: buffer {n} not found");
                return;
            }
            if n != 1 {
                self.push_jump();
            }
            self.switch_to_buffer(n - 1);
            return;
        }
        if let Some(p) = self.buffer.path() {
            if p.to_string_lossy().contains(spec) {
                return; // already active
            }
        }
        for (i, b) in self.other_buffers.iter().enumerate() {
            if let Some(p) = b.buffer.path() {
                if p.to_string_lossy().contains(spec) {
                    self.push_jump();
                    self.switch_to_buffer(i + 1);
                    return;
                }
            }
        }
        self.msg = format!("E86: no buffer matching \"{spec}\"");
    }
}
