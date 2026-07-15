use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use std::collections::HashMap;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vix_core::{
    apply_motion, compile_search, find_all_in_lines, find_backward, find_forward,
    handle_normal_char, text_object_range, Action, Buffer, Case, Change, FindDirection, FindKind,
    History, InsertPos, JumpEntry, JumpList, Mode, Motion, NormalKeyState, PendingOp, RepeatAction,
    SearchDirection, Selection, TextObject, TextObjectKind, Transaction,
};

mod completion;
pub mod help;
mod lsp;
mod picker;
pub mod testing;
mod util;
use vix_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};
use vix_lsp::{LspClient, RequestId};
use vix_syntax::{HlSpan, Language, SyntaxState, HIGHLIGHT_NAMES};

use completion::CompletionPopup;
use lsp::{diag_summary, sev_rank, LspDocState, PendingRequest};
use picker::render::{render_picker, render_picker_fullscreen};
use picker::{is_fullscreen_picker_kind, Picker, PickerItem};
use util::osc52_copy;

/// What action triggered the current Insert session — determines how `.`
/// will replay it on Esc.
#[derive(Debug, Clone)]
pub(crate) enum InsertOrigin {
    /// `i/a/I/A/o/O` — bare insert mode entry.
    Plain,
    /// `c<motion>` — replay re-evaluates the motion at the cursor.
    ChangeMotion { motion: Motion, count: usize },
    /// `c<text-object>` — replay re-resolves the text object at the cursor.
    ChangeObject {
        object: TextObject,
        kind: TextObjectKind,
    },
    /// `cc` (or `Ncc`) — replay deletes that many lines' content in place.
    ChangeLine { count: usize },
}

/// Accumulates text typed during an Insert-mode session, plus how that session
/// was entered. On Esc we commit this as one undo unit and one `.` repeat.
pub(crate) struct PendingInsert {
    pos: InsertPos,
    tx: Transaction,
    typed: String,
    /// What action started this insert session.
    origin: InsertOrigin,
}

/// Contents of the unnamed register (`"`), plus whether the last yank/delete
/// was linewise — determines how `p`/`P` paste.
#[derive(Debug, Clone, Default)]
pub(crate) struct Register {
    text: String,
    linewise: bool,
}

/// Snapshot of everything per-buffer: used to park inactive buffers while
/// another is active. Switching buffers swaps one of these with the fields
/// living directly on `Editor`.
pub(crate) struct BufferSave {
    buffer: Buffer,
    sel: Selection,
    history: History,
    view_top: usize,
    syntax: Option<SyntaxState>,
    syntax_cache: Vec<HlSpan>,
    syntax_version: Option<u64>,
    pending_insert: Option<PendingInsert>,
    last_change: Option<RepeatAction>,
    /// Stable creation-order id. Survives swaps; used to render a steady
    /// position counter in the statusline as the user cycles buffers.
    bid: u64,
}

pub struct Editor {
    pub buffer: Buffer,
    pub sel: Selection,
    pub mode: Mode,
    pub(crate) keys: NormalKeyState,
    pub(crate) history: History,
    pub(crate) pending_insert: Option<PendingInsert>,
    pub(crate) last_change: Option<RepeatAction>,
    /// Non-active buffers. Switching swaps the active set with one of these.
    pub(crate) other_buffers: Vec<BufferSave>,
    pub(crate) register: Register,
    /// Top line of the viewport (for vertical scrolling).
    pub view_top: usize,
    /// Command-line input buffer (active in Command and Search modes).
    pub cmdline: String,
    /// ':' for ex, '/' for forward search, '?' for backward.
    pub cmdline_prompt: char,
    /// Last search query (compiled). None when no search has been run.
    pub(crate) last_search: Option<(String, SearchDirection)>,
    /// Whether to render match highlights (cleared by :noh).
    pub(crate) hl_search: bool,
    /// Last char-find for `;`/`,` repeat.
    pub(crate) last_find: Option<(char, FindDirection, FindKind)>,
    /// Short status message shown at the right of the statusline.
    pub msg: String,
    pub quit: bool,
    /// Syntax highlighter, set if we recognized the file's language.
    pub(crate) syntax: Option<SyntaxState>,
    /// Cached highlight spans. Refreshed only when `syntax_version` lags
    /// behind `buffer.version()` — avoids reparsing on pure navigation.
    pub(crate) syntax_cache: Vec<HlSpan>,
    /// Buffer version the cache was computed against. `None` forces a rebuild
    /// on first use.
    pub(crate) syntax_version: Option<u64>,
    /// Active picker overlay (file finder / grep). Intercepts input while set.
    pub(crate) picker: Option<Picker>,
    /// Registered LSP clients keyed by `cmd`. We spawn lazily — one client per
    /// language per editor lifetime — and route per-buffer requests based on
    /// the file's extension.
    pub(crate) lsp_clients: HashMap<String, LspClient>,
    /// Per-buffer LSP document state: URI, monotonic version, latest
    /// buffer-version we sent to the server (to decide when to `didChange`).
    pub(crate) lsp_docs: HashMap<PathBuf, LspDocState>,
    /// Diagnostics per buffer path.
    pub(crate) diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    /// In-flight LSP request bookkeeping — maps server cmd + id → intent.
    /// Intent carries the buffer path for correlation after response arrives.
    pub(crate) pending_requests: HashMap<(String, RequestId), PendingRequest>,
    /// Server cmds we've already tried to spawn and failed on. Prevents
    /// re-spawning a missing binary on every K/gd.
    pub(crate) lsp_failed: std::collections::HashSet<String>,
    /// Per-server timestamps of recent crashes — keyed by server cmd. Used to
    /// rate-limit auto-restart (max 3 restarts per 60s before giving up).
    pub(crate) lsp_restart_log: HashMap<String, Vec<Instant>>,
    /// Bottom-area hover popup, set after a hover response arrives. Cleared
    /// by any subsequent keypress in Normal mode.
    pub(crate) hover_popup: Option<String>,
    /// Active completion popup in Insert mode, if any.
    pub(crate) completion_popup: Option<CompletionPopup>,
    /// Pending code actions awaiting user selection. Cleared when the picker
    /// closes.
    pub(crate) pending_code_actions: Vec<vix_lsp::lsp_types::CodeActionOrCommand>,
    /// Transient flash overlay after a yank — (range, expires_at).
    pub(crate) yank_flash: Option<(std::ops::Range<usize>, Instant)>,
    /// Jump-list ring for `Ctrl-O` / `Ctrl-I`. Entries are keyed by path + line
    /// + col so they survive buffer-index reshuffles and edits.
    pub(crate) jumps: JumpList,
    /// In Visual mode, the pending text-object kind from the last `i` / `a`.
    /// Cleared once the object char arrives or Esc is pressed.
    pub(crate) visual_object_kind: Option<TextObjectKind>,
    /// Stable id of the currently-active buffer. Paired with `BufferSave::bid`
    /// to render a position counter that tracks the active buffer through
    /// `<Tab>` / `:bn` rotations.
    pub(crate) active_bid: u64,
    /// Monotonic source for new buffer ids.
    pub(crate) next_bid: u64,
    /// Last rendered content rect. Used to translate mouse coords to buffer
    /// positions. None until the first frame is drawn.
    pub(crate) last_content_rect: Option<Rect>,
    /// Width of the gutter (line numbers + diag glyph + space) at the last
    /// render. Click x − content_rect.x − this = column into the line.
    pub(crate) last_gutter_cols: u16,
    /// Last rendered picker overlay rect, and the scroll offset into the
    /// match list at that frame. Used to translate mouse events on the picker
    /// back into list-item indices. `None` when no picker is up.
    pub(crate) last_picker_rect: Option<Rect>,
    pub(crate) last_picker_scroll: usize,
    pub(crate) last_picker_list_rows: usize,
    /// Set true after `<Space>` is pressed in Normal mode. The next key
    /// resolves the leader sequence. Cleared on Esc / mode changes / Ctrl-C.
    pub(crate) pending_leader: bool,
    /// One-shot flag used at launch: when the user opens vix without a file
    /// (or with a directory), we boot with an empty placeholder buffer and
    /// pop the file picker. The first buffer they pick should *replace*
    /// that placeholder rather than park it. Consumed on the first swap.
    pub(crate) discard_active_on_swap: bool,
    /// One `SyntaxState` per language, reused across picker preview rebuilds
    /// so we don't re-compile tree-sitter highlight queries on every j/k.
    /// The active buffer keeps its own `syntax` field; this cache exists
    /// purely for the picker's preview pane and any other ad-hoc highlights.
    pub(crate) preview_syntax: HashMap<Language, SyntaxState>,
    /// Latest grep generation. Bumped every time we issue a new background
    /// grep request; in-flight worker threads compare against this and
    /// bail (`WalkState::Quit`) when a newer generation appears, so a fresh
    /// keystroke supersedes the previous walk without waiting.
    pub(crate) grep_gen: Arc<std::sync::atomic::AtomicU64>,
    /// Receiver for the most recently spawned async grep worker. `None`
    /// when no grep is in flight. Pumped each tick from the run loop so
    /// results land on `picker.items` without blocking the UI thread.
    pub(crate) grep_pending: Option<std::sync::mpsc::Receiver<Vec<PickerItem>>>,
}

impl Editor {
    pub fn new(buffer: Buffer) -> Self {
        let syntax = buffer
            .path()
            .and_then(Language::from_path)
            .and_then(|lang| SyntaxState::new(lang).ok());
        Self {
            buffer,
            sel: Selection::at(0),
            mode: Mode::Normal,
            keys: NormalKeyState::default(),
            history: History::new(),
            pending_insert: None,
            last_change: None,
            other_buffers: Vec::new(),
            register: Register::default(),
            view_top: 0,
            cmdline: String::new(),
            cmdline_prompt: ':',
            last_search: None,
            hl_search: false,
            last_find: None,
            msg: String::new(),
            quit: false,
            syntax,
            syntax_cache: Vec::new(),
            syntax_version: None,
            picker: None,
            lsp_clients: HashMap::new(),
            lsp_docs: HashMap::new(),
            diagnostics: HashMap::new(),
            pending_requests: HashMap::new(),
            lsp_failed: std::collections::HashSet::new(),
            lsp_restart_log: HashMap::new(),
            hover_popup: None,
            completion_popup: None,
            pending_code_actions: Vec::new(),
            yank_flash: None,
            jumps: JumpList::default(),
            visual_object_kind: None,
            last_content_rect: None,
            last_gutter_cols: 0,
            last_picker_rect: None,
            last_picker_scroll: 0,
            last_picker_list_rows: 0,
            pending_leader: false,
            discard_active_on_swap: false,
            active_bid: 0,
            next_bid: 1,
            preview_syntax: HashMap::new(),
            grep_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            grep_pending: None,
        }
    }

    // --- Read-only accessors for tests / harness consumers --------------------
    pub fn parked_count(&self) -> usize {
        self.other_buffers.len()
    }
    pub fn buffer_count(&self) -> usize {
        self.other_buffers.len() + 1
    }
    pub fn register_text(&self) -> &str {
        &self.register.text
    }
    pub fn register_linewise(&self) -> bool {
        self.register.linewise
    }
    pub fn jump_list(&self) -> &JumpList {
        &self.jumps
    }
    pub fn last_change(&self) -> Option<&RepeatAction> {
        self.last_change.as_ref()
    }
    pub fn diagnostics_for_active(&self) -> usize {
        self.buffer
            .path()
            .and_then(|p| self.diagnostics.get(p))
            .map(|d| d.len())
            .unwrap_or(0)
    }
    pub fn active_language(&self) -> Option<vix_syntax::Language> {
        self.syntax.as_ref().map(|s| s.language())
    }
    pub fn symbol_names(&self) -> Vec<String> {
        let Some(s) = self.syntax.as_ref() else {
            return Vec::new();
        };
        let src = self.buffer.rope().to_string();
        s.symbols(src.as_bytes())
            .ok()
            .map(|v| v.into_iter().map(|sym| sym.name).collect())
            .unwrap_or_default()
    }

    /// Load `path` as a new buffer (or switch to it if already open). The
    /// previous active buffer is parked, including if it has unsaved edits
    /// — Vim-style `hidden`.
    /// Open a help topic in a scratch buffer. `topic` may be empty to show
    /// the index page. Subsequent `:help <same>` calls switch back to the
    /// existing buffer instead of duplicating it (path-keyed dedup).
    pub(crate) fn open_help_doc(&mut self, topic: &str) {
        let topic = topic.trim();
        let (slug, body) = if topic.is_empty() {
            ("index".to_string(), help::index())
        } else if let Some(t) = help::lookup(topic) {
            (t.slug.to_string(), t.body.to_string())
        } else {
            self.msg = format!("no help topic \"{topic}\" — try :help for the index");
            return;
        };
        // Synthetic path: brackets keep it visually distinct, `.md` extension
        // routes the markdown highlighter via `Language::from_path`.
        let synthetic = std::path::PathBuf::from(format!("[help:{slug}].md"));
        if let Some(idx) = self.buffer_index_by_path(&synthetic) {
            self.push_jump();
            self.switch_to_buffer(idx);
            return;
        }
        let mut buf = Buffer::from_text(&body);
        buf.set_path(&synthetic);
        buf.set_scratch(true);
        self.push_jump();
        self.add_or_switch_buffer(buf);
        self.msg = format!("help: {slug}");
    }

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

    /// Refresh `syntax_cache` if the buffer has mutated since the last parse.
    /// Cheap fast path when the user is just navigating (no edits).
    pub(crate) fn refresh_syntax_cache(&mut self) {
        let version = self.buffer.version();
        if self.syntax_version == Some(version) {
            return;
        }
        self.syntax_cache = if let Some(s) = self.syntax.as_mut() {
            let src = self.buffer.rope().to_string();
            s.highlight(src.as_bytes()).unwrap_or_default()
        } else {
            Vec::new()
        };
        self.syntax_version = Some(version);
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

    /// Returns the line index of the match on success so the caller can
    /// adjust the viewport (e.g. pin the first match to the top of the
    /// pane on a fresh `/` search).
    pub(crate) fn do_search(&mut self, query: &str, dir: SearchDirection) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        let re = match compile_search(query, Case::Smart) {
            Ok(r) => r,
            Err(e) => {
                self.msg = format!("E: {e}");
                return None;
            }
        };
        // Vim starts search from cursor + 1 for forward, cursor for backward.
        let start_from = match dir {
            SearchDirection::Forward => (self.sel.head + 1).min(self.buffer.len_chars()),
            SearchDirection::Backward => self.sel.head,
        };
        let hit = match dir {
            SearchDirection::Forward => find_forward(&self.buffer, &re, start_from)
                .or_else(|| find_forward(&self.buffer, &re, 0)), // wrap
            SearchDirection::Backward => find_backward(&self.buffer, &re, start_from)
                .or_else(|| find_backward(&self.buffer, &re, self.buffer.len_chars())),
        };
        match hit {
            Some((s, _)) => {
                self.sel = Selection::at(s).clamped(&self.buffer);
                self.last_search = Some((query.to_string(), dir));
                self.hl_search = true;
                Some(self.cursor_line())
            }
            None => {
                self.msg = format!("E486: Pattern not found: {query}");
                None
            }
        }
    }

    pub(crate) fn word_search_under(&mut self, dir: SearchDirection) {
        let rope = self.buffer.rope();
        let len = self.buffer.len_chars();
        if len == 0 {
            return;
        }
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let mut start = self.sel.head.min(len.saturating_sub(1));
        // If cursor is not on a word char, try to find one to the right on this line.
        if !is_word(rope.char(start)) {
            let (line, _) = self.buffer.char_to_line_col(start);
            let line_end = self.buffer.line_to_char(line) + self.buffer.line_len_chars(line);
            let mut i = start;
            while i < line_end && !is_word(rope.char(i)) {
                i += 1;
            }
            if i >= line_end {
                self.msg = "E348: No string under cursor".into();
                return;
            }
            start = i;
        }
        // Extend backward to start of word.
        while start > 0 && is_word(rope.char(start - 1)) {
            start -= 1;
        }
        let mut end = start;
        while end < len && is_word(rope.char(end)) {
            end += 1;
        }
        let word: String = rope.slice(start..end).to_string();
        // Build a pattern with word boundaries, escaping regex metachars.
        let escaped = regex_escape_like(&word);
        let pattern = format!(r"\b{escaped}\b");
        self.do_search(&pattern, dir);
    }

    pub(crate) fn search_repeat(&mut self, dir: SearchDirection) {
        let Some((query, last_dir)) = self.last_search.clone() else {
            self.msg = "No previous search".into();
            return;
        };
        // `n` repeats in original direction; `N` reverses it.
        let effective = match (last_dir, dir) {
            (d, SearchDirection::Forward) => d,
            (SearchDirection::Forward, SearchDirection::Backward) => SearchDirection::Backward,
            (SearchDirection::Backward, SearchDirection::Backward) => SearchDirection::Forward,
        };
        self.hl_search = true;
        self.do_search(&query, effective);
    }

    pub(crate) fn cursor_line(&self) -> usize {
        self.buffer.char_to_line_col(self.sel.head).0
    }

    /// Char-range covered by the current Visual/VisualLine selection, ready
    /// to be consumed by an operator.
    pub(crate) fn visual_range(&self) -> std::ops::Range<usize> {
        let r = self.sel.range();
        match self.mode {
            Mode::Visual => {
                // Charwise and inclusive of the head char (Vim-style).
                let end = (r.end + 1).min(self.buffer.len_chars());
                r.start..end
            }
            Mode::VisualLine => {
                let (start_line, _) = self.buffer.char_to_line_col(r.start);
                let (end_line, _) = self.buffer.char_to_line_col(r.end);
                let start = self.buffer.line_to_char(start_line);
                let end = self.buffer.line_to_char(end_line) + self.buffer.line_len_chars(end_line);
                // Include trailing newline if present.
                if end < self.buffer.len_chars() {
                    start..(end + 1)
                } else {
                    start..end
                }
            }
            _ => r,
        }
    }

    pub(crate) fn ensure_cursor_visible(&mut self, viewport_rows: usize) {
        let line = self.cursor_line();
        if line < self.view_top {
            self.view_top = line;
        } else if line >= self.view_top + viewport_rows {
            self.view_top = line + 1 - viewport_rows;
        }
    }

    /// Lines to jump on PageUp / PageDown — vim's `<C-f>`/`<C-b>` convention
    /// (viewport height − 2, leaving two rows of context across the jump).
    /// Falls back to 10 if no frame has rendered yet.
    pub(crate) fn page_step(&self) -> usize {
        match self.last_content_rect {
            Some(r) => (r.height as usize).saturating_sub(2).max(1),
            None => 10,
        }
    }

    pub(crate) fn run_ex(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        match cmd {
            "w" => self.format_and_save(),
            "fmt" | "format" => {
                self.format_buffer();
            }
            "action" | "actions" | "ca" => {
                self.run_code_action();
            }
            "q" => {
                if self.buffer.dirty() {
                    self.msg = "E37: No write since last change (use :q!)".into();
                } else {
                    self.close_buffer(false);
                }
            }
            "q!" => {
                self.close_buffer(true);
            }
            "qa" | "qall" => {
                if self.any_buffer_dirty() {
                    self.msg = "E37: unsaved buffers exist (use :qa!)".into();
                } else {
                    self.quit = true;
                }
            }
            "qa!" | "qall!" => {
                self.quit = true;
            }
            "wq" | "x" => {
                self.format_and_save();
                if !self.buffer.dirty() {
                    self.close_buffer(false);
                }
            }
            "noh" | "nohl" | "nohlsearch" => {
                self.hl_search = false;
            }
            "Files" => {
                self.open_files_picker();
            }
            "Symbols" => {
                self.open_symbols_picker();
            }
            "Buffers" | "ls" => {
                self.open_buffers_picker();
            }
            "jumps" => {
                self.open_jumps_picker();
            }
            "help" | "h" => {
                self.open_help_doc("");
            }
            "bn" | "bnext" => {
                self.next_buffer();
            }
            "bp" | "bprev" | "bprevious" => {
                self.prev_buffer();
            }
            "bd" | "bdelete" => {
                self.close_buffer(false);
            }
            "bd!" | "bdelete!" => {
                self.close_buffer(true);
            }
            "e" | "edit" => {
                self.reload_buffer(false);
            }
            "e!" | "edit!" => {
                self.reload_buffer(true);
            }
            "" => {}
            _ => {
                if let Some(rest) = cmd.strip_prefix("Grep") {
                    self.open_grep_picker(rest.trim());
                } else if let Some(rest) =
                    cmd.strip_prefix("help ").or_else(|| cmd.strip_prefix("h "))
                {
                    self.open_help_doc(rest.trim());
                } else if let Some(rest) =
                    cmd.strip_prefix("e ").or_else(|| cmd.strip_prefix("e! "))
                {
                    let path = std::path::PathBuf::from(rest.trim());
                    self.open_path(&path);
                } else if let Some(rest) = cmd.strip_prefix("b ") {
                    self.switch_buffer_by_spec(rest.trim());
                } else if let Some(new_name) = cmd.strip_prefix("rename ").map(str::trim) {
                    if new_name.is_empty() {
                        self.msg = "usage: :rename <new-name>".into();
                    } else {
                        self.run_rename(new_name);
                    }
                } else if cmd.starts_with("%s") || cmd.starts_with(".s") || cmd.starts_with('s') {
                    self.run_substitute(cmd);
                } else {
                    self.msg = format!("not implemented: :{cmd}");
                }
            }
        }
    }

    /// Parse and execute `:%s/pat/rep/flags`, `:s/pat/rep/flags`, etc.
    /// v1 supports `%` (whole file) and no-range (current line). Flags: g, i.
    pub(crate) fn run_substitute(&mut self, cmd: &str) {
        // Strip the range prefix + the `s`.
        let rest = if let Some(r) = cmd.strip_prefix("%s") {
            r
        } else if let Some(r) = cmd.strip_prefix(".s") {
            r
        } else if let Some(r) = cmd.strip_prefix('s') {
            r
        } else {
            self.msg = "internal: bad :s".into();
            return;
        };
        let whole_file = cmd.starts_with("%s");

        // First char after `s` is the delimiter.
        let mut chars = rest.chars();
        let Some(delim) = chars.next() else {
            self.msg = "E471: usage :s/pat/rep/flags".into();
            return;
        };
        let body: String = chars.collect();
        let parts: Vec<&str> = body.splitn(3, delim).collect();
        if parts.len() < 2 {
            self.msg = "E471: usage :s/pat/rep/flags".into();
            return;
        }
        let pattern = parts[0];
        let replacement = parts[1];
        let flags = parts.get(2).copied().unwrap_or("");
        let global = flags.contains('g');
        let case_insensitive = flags.contains('i');

        let re = match regex::RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                self.msg = format!("E: {e}");
                return;
            }
        };

        // Determine the char-range to operate on.
        let (range_start, range_end) = if whole_file {
            (0, self.buffer.len_chars())
        } else {
            let line = self.cursor_line();
            let s = self.buffer.line_to_char(line);
            let e = s + self.buffer.line_len_chars(line);
            (s, e)
        };

        // Collect matches line-by-line so we can replace inside each line with
        // Vim-ish semantics (per-line `g` flag). Must walk lines forward but
        // apply replacements in reverse across the whole buffer so offsets
        // stay valid.
        let (start_line, _) = self.buffer.char_to_line_col(range_start);
        let end_line_exclusive = if range_end == 0 {
            0
        } else {
            self.buffer.char_to_line_col(range_end.saturating_sub(1)).0 + 1
        };

        let mut replacements: Vec<(std::ops::Range<usize>, String, String)> = Vec::new();
        for line in start_line..end_line_exclusive {
            let line_text: String = self.buffer.rope().line(line).chars().collect();
            let line_text = line_text.trim_end_matches('\n');
            let line_start = self.buffer.line_to_char(line);
            let mut offset = 0usize; // byte offset within line_text
            for m in re.find_iter(line_text) {
                let start_byte = m.start();
                let end_byte = m.end();
                let matched = &line_text[start_byte..end_byte];
                let rep = re.replace(matched, replacement).to_string();
                let start_char = line_start + line_text[..start_byte].chars().count();
                let end_char = line_start + line_text[..end_byte].chars().count();
                replacements.push((start_char..end_char, matched.to_string(), rep));
                offset = end_byte;
                if !global {
                    break;
                }
            }
            let _ = offset;
        }

        if replacements.is_empty() {
            self.msg = format!("E486: Pattern not found: {pattern}");
            return;
        }

        // Apply in reverse char-offset order so earlier offsets stay valid.
        replacements.sort_by(|a, b| b.0.start.cmp(&a.0.start));
        let sel_before = self.sel;
        let mut tx = Transaction::new();
        tx.sel_before = Some(sel_before);
        let count = replacements.len();
        for (range, old, new_text) in &replacements {
            self.buffer.remove_range(range.clone());
            self.buffer.insert_str(range.start, new_text);
            tx.push(Change::Delete {
                at: range.start,
                removed: old.clone(),
            });
            tx.push(Change::Insert {
                at: range.start,
                text: new_text.clone(),
            });
        }
        self.sel = self.sel.clamped(&self.buffer);
        tx.sel_after = Some(self.sel);
        self.history.commit(tx);
        self.msg = format!("{count} substitutions");
    }

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
        self.sel =
            Selection::at(range.start + new_len.saturating_sub(1).max(0)).clamped(&self.buffer);
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
            cursor_after = insert_at + n.saturating_sub(1).max(0);
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

    #[doc(hidden)]
    pub fn set_render_geometry_for_test(&mut self, rect: Rect, gutter_cols: u16) {
        self.last_content_rect = Some(rect);
        self.last_gutter_cols = gutter_cols;
    }

    #[doc(hidden)]
    pub fn set_picker_geometry_for_test(&mut self, rect: Rect, scroll: usize) {
        self.last_picker_rect = Some(rect);
        self.last_picker_scroll = scroll;
        self.last_picker_list_rows = rect.height.saturating_sub(1) as usize;
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

pub(crate) fn regex_escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".+*?()[]{}|^$\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(crate) fn render(f: &mut ratatui::Frame, ed: &mut Editor) {
    let area = f.area();

    // Files / Grep / Buffers picker takes the whole screen — skip drawing
    // the editor, statusline, and cmdline so we don't peek through.
    let fullscreen_picker = ed
        .picker
        .as_ref()
        .map(|p| is_fullscreen_picker_kind(&p.kind))
        .unwrap_or(false);
    if fullscreen_picker {
        // LSP sync still useful — keeps server state consistent across the
        // (potentially long) picker session.
        ed.sync_lsp_changes();
        render_picker_fullscreen(f, area, ed);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1), // statusline
            Constraint::Length(1), // cmdline / messages
        ])
        .split(area);

    let content_area = chunks[0];
    let statusline_area = chunks[1];
    let cmdline_area = chunks[2];

    ed.ensure_cursor_visible(content_area.height as usize);
    ed.refresh_syntax_cache();
    // Push any pending text changes to the LSP server before rendering. The
    // server's response (diagnostics) lands in the next event drain.
    ed.sync_lsp_changes();
    // Decay transient yank-flash overlay.
    if let Some((_, until)) = ed.yank_flash.as_ref() {
        if Instant::now() >= *until {
            ed.yank_flash = None;
        }
    }

    // Take the highlight cache out of `ed` so we can pass `&mut ed` and the
    // borrowed cache through render_content side by side. Restored after.
    let hl_cache = std::mem::take(&mut ed.syntax_cache);
    render_content(f, content_area, ed, &hl_cache);
    ed.syntax_cache = hl_cache;
    render_statusline(f, statusline_area, ed);
    render_cmdline(f, cmdline_area, ed);

    if ed.hover_popup.is_some() {
        render_hover(f, content_area, ed);
    }
    if ed.completion_popup.is_some() {
        render_completion_popup(f, content_area, ed);
    }
    if ed.picker.is_some() {
        render_picker(f, content_area, ed);
    } else {
        ed.last_picker_rect = None;
        ed.last_picker_list_rows = 0;
    }
}

pub(crate) fn render_hover(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    let Some(text) = ed.hover_popup.as_deref() else {
        return;
    };
    let max_w = (area.width as u32 * 2 / 3).max(30).min(area.width as u32) as u16;
    // Wrap text to max_w - 2 (1 col padding each side).
    let inner_w = max_w.saturating_sub(2).max(10) as usize;
    let mut wrapped: Vec<String> = Vec::new();
    for raw in text.lines() {
        if raw.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let mut remaining = raw;
        while !remaining.is_empty() {
            let take = remaining.chars().take(inner_w).collect::<String>();
            let n = take.chars().count();
            wrapped.push(take);
            remaining = &remaining[remaining
                .char_indices()
                .nth(n)
                .map(|(i, _)| i)
                .unwrap_or(remaining.len())..];
        }
    }
    let h = (wrapped.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(3);
    let w = max_w;
    let x = area.x + area.width.saturating_sub(w) - 1;
    let y = area.y + 1;
    let rect = Rect::new(x, y, w, h);

    let bg = Style::default().bg(Color::DarkGray).fg(Color::White);
    let blank: Vec<Line> = (0..h).map(|_| Line::raw(" ".repeat(w as usize))).collect();
    f.render_widget(Paragraph::new(blank).style(bg), rect);

    let mut lines: Vec<Line> = Vec::with_capacity(h as usize);
    lines.push(Line::styled(
        " hover ".to_string() + &" ".repeat((w as usize).saturating_sub(7)),
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    for l in wrapped.iter().take((h as usize).saturating_sub(2)) {
        let pad = (w as usize).saturating_sub(l.chars().count() + 1);
        lines.push(Line::from(vec![Span::styled(
            format!(" {}{}", l, " ".repeat(pad)),
            bg,
        )]));
    }
    while lines.len() < h as usize {
        lines.push(Line::styled(" ".repeat(w as usize), bg));
    }
    f.render_widget(Paragraph::new(lines), rect);
}

/// Draw the completion popup anchored to the cursor. Opens below the cursor
/// if there's room, else above.
pub(crate) fn render_completion_popup(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    use vix_lsp::lsp_types::CompletionItemKind;
    let Some(popup) = ed.completion_popup.as_ref() else {
        return;
    };
    if popup.visible.is_empty() {
        return;
    }

    let (cursor_line, cursor_col) = ed.buffer.char_to_line_col(ed.sel.head);
    let total_lines = ed.buffer.len_lines();
    let gutter_width = total_lines.to_string().len().max(3) + 1;

    // Screen position of the cursor char (top-left of its cell).
    let screen_row = cursor_line.saturating_sub(ed.view_top);
    let screen_col = gutter_width + 2 + cursor_col;
    let anchor_x = area.x + screen_col as u16;
    let anchor_y = area.y + screen_row as u16;

    // Size the popup: up to 8 rows, width = longest label + kind badge.
    let max_rows = 8u16;
    let n = popup.visible.len() as u16;
    let rows = n.min(max_rows);
    let max_label = popup
        .visible
        .iter()
        .map(|&i| popup.items[i].label.chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 40);
    let width = (max_label as u16 + 4).min(area.width.saturating_sub(2));

    // Place below cursor if there's room; otherwise above.
    let below_room = area.height.saturating_sub(anchor_y - area.y + 1);
    let (y, h) = if below_room >= rows {
        (anchor_y + 1, rows)
    } else {
        let above_room = anchor_y.saturating_sub(area.y);
        let h = rows.min(above_room);
        (anchor_y.saturating_sub(h), h)
    };
    let x = anchor_x.min(area.x + area.width.saturating_sub(width));
    if h == 0 || width == 0 {
        return;
    }
    let rect = Rect::new(x, y, width, h);

    let bg = Style::default().bg(Color::Rgb(30, 30, 40)).fg(Color::White);
    let sel_bg = Style::default()
        .bg(Color::Blue)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);

    // Scroll the visible slice so `selected` is always in view.
    let start = popup
        .selected
        .saturating_sub(h as usize - 1)
        .min(popup.visible.len().saturating_sub(h as usize));
    let end = (start + h as usize).min(popup.visible.len());

    let mut lines: Vec<Line> = Vec::with_capacity(h as usize);
    for row in start..end {
        let item_idx = popup.visible[row];
        let item = &popup.items[item_idx];
        let kind_badge = match item.kind {
            Some(CompletionItemKind::FUNCTION) | Some(CompletionItemKind::METHOD) => "f",
            Some(CompletionItemKind::VARIABLE) | Some(CompletionItemKind::FIELD) => "v",
            Some(CompletionItemKind::CLASS) | Some(CompletionItemKind::STRUCT) => "t",
            Some(CompletionItemKind::ENUM) | Some(CompletionItemKind::ENUM_MEMBER) => "e",
            Some(CompletionItemKind::MODULE) => "m",
            Some(CompletionItemKind::KEYWORD) => "k",
            Some(CompletionItemKind::SNIPPET) => "s",
            _ => " ",
        };
        let label_w = (width as usize).saturating_sub(4);
        let label = item.label.chars().take(label_w).collect::<String>();
        let pad = (width as usize).saturating_sub(label.chars().count() + 4);
        let text = format!(" {} {}{} ", kind_badge, label, " ".repeat(pad));
        let style = if row == popup.selected { sel_bg } else { bg };
        lines.push(Line::from(Span::styled(text, style)));
    }
    f.render_widget(Paragraph::new(lines).style(bg), rect);
}

/// Translate a tree-sitter highlight scope index into a ratatui style.
/// Returns `None` for unstyled ("default foreground") scopes.
pub(crate) fn scope_style(scope_idx: usize) -> Option<Style> {
    let name = HIGHLIGHT_NAMES.get(scope_idx)?;
    let color = if name.starts_with("keyword") {
        Color::Magenta
    } else if name.starts_with("function") {
        Color::LightBlue
    } else if name.starts_with("type") {
        Color::Cyan
    } else if name.starts_with("string") {
        Color::LightYellow
    } else if name.starts_with("constant") {
        Color::LightRed
    } else {
        match *name {
            "comment" => Color::DarkGray,
            "attribute" | "constructor" => Color::LightMagenta,
            "namespace" | "label" => Color::Yellow,
            "property" => Color::LightCyan,
            "tag" => Color::LightGreen,
            _ => return None,
        }
    };
    Some(Style::default().fg(color))
}

pub(crate) fn render_content(
    f: &mut ratatui::Frame,
    area: Rect,
    ed: &mut Editor,
    hl_spans: &[HlSpan],
) {
    let total_lines = ed.buffer.len_lines();
    let rows = area.height as usize;
    let gutter_width = total_lines.to_string().len().max(3) + 1;
    // Stash for mouse → buffer translation. `+2` = diag-glyph col + trailing
    // space before content. Matches the prefix actually written below.
    ed.last_content_rect = Some(area);
    ed.last_gutter_cols = (gutter_width + 2) as u16;

    let (cursor_line, cursor_col) = ed.buffer.char_to_line_col(ed.sel.head);

    // Per-line diagnostic severity map for the active buffer.
    let diag_by_line: HashMap<usize, DiagnosticSeverity> = {
        let mut m: HashMap<usize, DiagnosticSeverity> = HashMap::new();
        if let Some(path) = ed.buffer.path() {
            if let Some(diags) = ed.diagnostics.get(path) {
                for d in diags {
                    let line = d.range.start.line as usize;
                    let sev = d.severity.unwrap_or(DiagnosticSeverity::INFORMATION);
                    m.entry(line)
                        .and_modify(|cur| {
                            if sev_rank(sev) > sev_rank(*cur) {
                                *cur = sev;
                            }
                        })
                        .or_insert(sev);
                }
            }
        }
        m
    };

    // Compute search highlight ranges for the visible window.
    let highlights: Vec<(usize, usize)> = if ed.hl_search {
        if let Some((q, _)) = ed.last_search.as_ref() {
            if let Ok(re) = compile_search(q, Case::Smart) {
                find_all_in_lines(&ed.buffer, &re, ed.view_top, ed.view_top + rows)
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for screen_row in 0..rows {
        let line_idx = ed.view_top + screen_row;
        if line_idx >= total_lines {
            lines.push(Line::from(Span::styled(
                "~",
                Style::default().fg(Color::DarkGray),
            )));
            continue;
        }

        let num = format!("{:>width$} ", line_idx + 1, width = gutter_width - 1);
        let mut spans = vec![Span::styled(num, Style::default().fg(Color::DarkGray))];
        // Diagnostic indicator column (one char).
        let (glyph, color) = match diag_by_line.get(&line_idx) {
            Some(&DiagnosticSeverity::ERROR) => ("●", Color::Red),
            Some(&DiagnosticSeverity::WARNING) => ("●", Color::Yellow),
            Some(&DiagnosticSeverity::INFORMATION) => ("●", Color::Cyan),
            Some(&DiagnosticSeverity::HINT) => ("○", Color::Gray),
            Some(_) | None => (" ", Color::Reset),
        };
        spans.push(Span::styled(glyph.to_string(), Style::default().fg(color)));
        spans.push(Span::raw(" "));

        let line_start_char = ed.buffer.line_to_char(line_idx);
        let line_text: String = ed.buffer.rope().line(line_idx).chars().collect();
        let line_text = line_text.trim_end_matches('\n').to_string();
        let chars: Vec<char> = line_text.chars().collect();

        // Build per-char style overlay. Syntax highlighting is the base
        // layer; search/visual/cursor overlays override it.
        let mut styles: Vec<Option<Style>> = vec![None; chars.len()];

        // Apply syntax spans overlapping this line. Spans use byte offsets;
        // convert them to per-line char columns via the rope.
        if !hl_spans.is_empty() && !chars.is_empty() {
            let rope = ed.buffer.rope();
            let line_start_byte = rope.char_to_byte(line_start_char);
            let line_end_byte = line_start_byte + line_text.len();
            for span in hl_spans {
                if span.range.end <= line_start_byte || span.range.start >= line_end_byte {
                    continue;
                }
                let s_byte = span.range.start.max(line_start_byte);
                let e_byte = span.range.end.min(line_end_byte);
                let s_col = rope.byte_to_char(s_byte) - line_start_char;
                let e_col = rope.byte_to_char(e_byte) - line_start_char;
                let style = match scope_style(span.scope) {
                    Some(s) => s,
                    None => continue,
                };
                for slot in styles.iter_mut().take(e_col).skip(s_col) {
                    *slot = Some(style);
                }
            }
        }

        // Apply search highlights that overlap this line.
        let hl_style = Style::default().bg(Color::Yellow).fg(Color::Black);
        for &(s, e) in &highlights {
            let rel_s = s.saturating_sub(line_start_char);
            let rel_e = e.saturating_sub(line_start_char).min(chars.len());
            if rel_s >= chars.len() {
                continue;
            }
            for slot in styles.iter_mut().take(rel_e).skip(rel_s) {
                *slot = Some(hl_style);
            }
        }

        // Apply visual selection highlight (layered over search highlight).
        if matches!(ed.mode, Mode::Visual | Mode::VisualLine) {
            let vrange = ed.visual_range();
            let sel_style = Style::default().bg(Color::Blue).fg(Color::White);
            if vrange.start < line_start_char + chars.len() + 1 && vrange.end > line_start_char {
                let rel_s = vrange.start.saturating_sub(line_start_char);
                let rel_e = vrange.end.saturating_sub(line_start_char).min(chars.len());
                let rel_s = rel_s.min(chars.len());
                for slot in styles.iter_mut().take(rel_e).skip(rel_s) {
                    *slot = Some(sel_style);
                }
            }
        }

        // Yank-flash highlight: brief overlay on the yanked range.
        if let Some((yr, _)) = ed.yank_flash.as_ref() {
            let flash_style = Style::default().bg(Color::LightYellow).fg(Color::Black);
            if yr.start < line_start_char + chars.len() + 1 && yr.end > line_start_char {
                let rel_s = yr.start.saturating_sub(line_start_char).min(chars.len());
                let rel_e = yr.end.saturating_sub(line_start_char).min(chars.len());
                for slot in styles.iter_mut().take(rel_e).skip(rel_s) {
                    *slot = Some(flash_style);
                }
            }
        }

        // Build spans, merging consecutive equal styles.
        let mut i = 0;
        while i < chars.len() {
            let is_cursor = line_idx == cursor_line && i == cursor_col.min(chars.len());
            let base = styles[i];
            let style = if is_cursor {
                Some(match ed.mode {
                    Mode::Insert => Style::default()
                        .add_modifier(Modifier::UNDERLINED)
                        .fg(Color::White),
                    _ => Style::default().add_modifier(Modifier::REVERSED),
                })
            } else {
                base
            };
            // Cursor is a single char; otherwise merge consecutive equal-style runs.
            let mut j = i + 1;
            if !is_cursor {
                while j < chars.len()
                    && !(line_idx == cursor_line && j == cursor_col.min(chars.len()))
                    && styles[j] == base
                {
                    j += 1;
                }
            }
            let text: String = chars[i..j].iter().collect();
            match style {
                Some(s) => spans.push(Span::styled(text, s)),
                None => spans.push(Span::raw(text)),
            }
            i = j;
        }

        // If cursor is past the end of the line, draw a cursor placeholder.
        if line_idx == cursor_line && cursor_col >= chars.len() {
            let cursor_style = match ed.mode {
                Mode::Insert => Style::default()
                    .add_modifier(Modifier::UNDERLINED)
                    .fg(Color::White),
                _ => Style::default().add_modifier(Modifier::REVERSED),
            };
            spans.push(Span::styled(" ", cursor_style));
        }

        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), area);
}

pub(crate) fn render_statusline(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    let (line, col) = ed.buffer.char_to_line_col(ed.sel.head);
    let mode_style = match ed.mode {
        Mode::Normal => Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        Mode::Insert => Style::default()
            .bg(Color::Green)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
        Mode::Visual | Mode::VisualLine => Style::default()
            .bg(Color::Magenta)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
        Mode::Command => Style::default()
            .bg(Color::Yellow)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    };
    let path = ed
        .buffer
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "[No Name]".into());
    let dirty = if ed.buffer.dirty() { " [+]" } else { "" };
    let buf_count = ed.other_buffers.len() + 1;
    let buf_info = if buf_count > 1 {
        // Position by stable buffer id so the counter advances as the user
        // cycles with `<Tab>` / `:bn` instead of being pinned at 1.
        let mut bids: Vec<u64> = ed.other_buffers.iter().map(|b| b.bid).collect();
        bids.push(ed.active_bid);
        bids.sort_unstable();
        let pos = bids
            .iter()
            .position(|b| *b == ed.active_bid)
            .map(|i| i + 1)
            .unwrap_or(1);
        format!("[{pos}/{buf_count}] ")
    } else {
        String::new()
    };
    let diag_info = diag_summary(ed);
    let right = format!(" {}{}{}:{} ", buf_info, diag_info, line + 1, col + 1);
    let left_mode = format!(" {} ", ed.mode.label());
    let middle_pad = (area.width as usize)
        .saturating_sub(left_mode.len() + path.len() + dirty.len() + right.len() + 1);
    let middle = format!(" {}{}{}", path, dirty, " ".repeat(middle_pad));
    let line_widget = Line::from(vec![
        Span::styled(left_mode, mode_style),
        Span::styled(
            middle,
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        Span::styled(right, Style::default().bg(Color::DarkGray).fg(Color::White)),
    ]);
    f.render_widget(Paragraph::new(line_widget), area);
}

pub(crate) fn render_cmdline(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    let content = match ed.mode {
        Mode::Command => format!("{}{}", ed.cmdline_prompt, ed.cmdline),
        _ => ed.msg.clone(),
    };
    f.render_widget(Paragraph::new(content), area);
}

pub fn run(buffer: Buffer, open_files_picker: bool) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;

    let mut ed = Editor::new(buffer);
    ed.ensure_lsp_open();
    if open_files_picker {
        ed.discard_active_on_swap = true;
        ed.open_files_picker();
    }
    let result = (|| -> io::Result<()> {
        while !ed.quit {
            ed.drain_lsp_events();
            ed.flush_picker_query_if_due();
            ed.pump_grep_results();
            term.draw(|f| render(f, &mut ed))?;
            // Poll for input with a short timeout so LSP events get a chance
            // to flow in between keystrokes without blocking.
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => ed.handle_key(k),
                    Event::Mouse(m) => ed.handle_mouse(m),
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    // Always restore terminal even on error.
    terminal::disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        DisableMouseCapture,
        terminal::LeaveAlternateScreen
    )?;
    term.show_cursor()?;
    result
}
