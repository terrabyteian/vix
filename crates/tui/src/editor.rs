use ratatui::layout::Rect;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use vix_core::{
    Buffer, FindDirection, FindKind, History, InsertPos, JumpList, Mode, Motion, NormalKeyState,
    RepeatAction, SearchDirection, Selection, TextObject, TextObjectKind, Transaction,
};

use vix_lsp::lsp_types::Diagnostic;
use vix_lsp::{LspClient, RequestId};
use vix_syntax::{HlSpan, Language, SyntaxState};

use crate::buffers::BufferSave;
use crate::completion::CompletionPopup;
use crate::help;
use crate::lsp::{LspDocState, PendingRequest};
use crate::picker::{Picker, PickerItem};

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
    pub(crate) pos: InsertPos,
    pub(crate) tx: Transaction,
    pub(crate) typed: String,
    /// What action started this insert session.
    pub(crate) origin: InsertOrigin,
}

/// Contents of the unnamed register (`"`), plus whether the last yank/delete
/// was linewise — determines how `p`/`P` paste.
#[derive(Debug, Clone, Default)]
pub(crate) struct Register {
    pub(crate) text: String,
    pub(crate) linewise: bool,
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
}
