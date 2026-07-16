use ratatui::layout::Rect;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use vix_core::{
    Buffer, FindDirection, FindKind, History, InsertPos, JumpList, Mode, Motion, NormalKeyState,
    RepeatAction, SearchDirection, Selection, TextObject, TextObjectKind, Transaction,
};

use vix_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};
use vix_lsp::{LspClient, RequestId};
use vix_syntax::{HlSpan, Language, SyntaxState};

use crate::buffers::BufferSave;
use crate::completion::CompletionPopup;
use crate::help;
use crate::lsp::{LspDocState, PendingRequest};
use crate::picker::{PendingSource, Picker, PICKER_REFRESH_DEBOUNCE_MS};
use crate::theme::Theme;

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

/// Caches `render_content`'s search-highlight viewport scan so an unchanged
/// frame (same query, buffer contents, and viewport) doesn't recompile the
/// regex and rescan the visible lines every draw. Any field mismatch means
/// "recompute" — including `buffer_id`, which guards against a stale hit
/// when the active buffer is swapped (buffer `version()` counters restart
/// at 0 per-buffer, so version+viewport alone could coincidentally match
/// across two different buffers).
pub(crate) struct SearchHlCache {
    pub(crate) query: String,
    pub(crate) buffer_id: u64,
    pub(crate) buffer_version: u64,
    pub(crate) view_top: usize,
    pub(crate) rows: usize,
    /// Char-offset (start, end) match ranges, as returned by
    /// `find_all_in_lines`.
    pub(crate) ranges: Vec<(usize, usize)>,
    /// Compiled regex, kept alongside the ranges so a future `n`/`N` repeat
    /// could reuse it without recompiling. Not consulted for the cache-key
    /// comparison itself, and not read yet — `n`/`N` still recompile via
    /// `do_search`. Reserved for that follow-up.
    #[allow(dead_code)]
    pub(crate) re: regex::Regex,
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
    /// Byte window `syntax_cache` covers. `None` = whole file (the full
    /// highlight path). `Some(w)` = viewport-limited spans from the windowed
    /// query path; a viewport still inside `w` is a cache hit, scrolling
    /// past it re-queries (no reparse while the buffer version is
    /// unchanged).
    pub(crate) syntax_window: Option<std::ops::Range<usize>>,
    /// Reused scratch for the rope → contiguous-source copy handed to
    /// tree-sitter. Keyed by (buffer id, version) so buffer switches and
    /// edits rebuild it; navigation reuses it byte-for-byte.
    pub(crate) syntax_src: String,
    pub(crate) syntax_src_key: Option<(u64, u64)>,
    /// Active picker overlay (omni file/content search, buffers, symbols, …).
    /// Intercepts input while set.
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
    /// Bumped every time `diagnostics` is mutated. Part of `diag_line_cache`'s
    /// key, so a stale per-line severity map is never reused after new
    /// diagnostics arrive.
    pub(crate) diagnostics_gen: u64,
    /// Cached per-line diagnostic severity map for the active buffer, keyed
    /// by `(diagnostics_gen, path)`. Rebuilt only when diagnostics changed or
    /// the active buffer's path differs from the cached one.
    pub(crate) diag_line_cache: Option<(u64, PathBuf, HashMap<usize, DiagnosticSeverity>)>,
    /// Cached search-highlight viewport scan. See [`SearchHlCache`].
    pub(crate) search_hl_cache: Option<SearchHlCache>,
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
    /// One-shot flag used at launch: when the launch omnibox (see
    /// `discard_active_on_swap`) is dismissed via `PickerAction::Close`
    /// (Esc or Ctrl-C) with nothing picked, there's no file to edit, so we
    /// quit vix outright rather than leave the `[No Name]` placeholder on
    /// screen. Set only by the launch branch in `app::run`; every selection
    /// path (`Select`, `SelectMany`, mouse activation) clears it before
    /// dispatching so a pick always leaves the editor running normally, and
    /// no in-editor picker-open path (Ctrl-P, Space-f/g/b, `:Files` etc.)
    /// ever sets it.
    pub(crate) quit_on_picker_close: bool,
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
    /// Cancel token for the streaming file scan, independent of `grep_gen`
    /// so starting a new grep can't kill an in-flight scan (the scan keeps
    /// warming the Files list while the user greps).
    pub(crate) files_gen: Arc<std::sync::atomic::AtomicU64>,
    /// Streaming grep worker channel, if a grep is in flight. Pumped each
    /// tick from the run loop so batches land on `picker.grep_items`
    /// without blocking the UI thread.
    pub(crate) grep_source: Option<PendingSource>,
    /// Streaming file-scan worker channel, if a scan is in flight. Fills
    /// `picker.file_items` batch by batch — the picker opens instantly and
    /// the list grows under it.
    pub(crate) files_source: Option<PendingSource>,
    /// Recent-files store location, resolved once at startup (see
    /// `crate::recent::default_data_file`). `None` disables persistence —
    /// tests force this off via `Harness` so they never touch the real
    /// user file.
    pub(crate) recent_data_file: Option<std::path::PathBuf>,
    /// Centralized color/style palette. See `crate::theme`.
    pub(crate) theme: Theme,
    /// Dirty flag for the main loop: draw a frame only when something
    /// visible may have changed. Set by input events, LSP events, streaming
    /// picker batches, debounce flushes, resize, and yank-flash decay;
    /// cleared after each draw. Starts true so the first frame paints.
    pub(crate) needs_redraw: bool,
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
            syntax_window: None,
            syntax_src: String::new(),
            syntax_src_key: None,
            picker: None,
            lsp_clients: HashMap::new(),
            lsp_docs: HashMap::new(),
            diagnostics: HashMap::new(),
            diagnostics_gen: 0,
            diag_line_cache: None,
            search_hl_cache: None,
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
            quit_on_picker_close: false,
            active_bid: 0,
            next_bid: 1,
            preview_syntax: HashMap::new(),
            grep_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            files_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            grep_source: None,
            files_source: None,
            recent_data_file: crate::recent::default_data_file(),
            theme: Theme::default_dark(),
            needs_redraw: true,
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

    /// Mark the screen dirty: the next main-loop iteration draws a frame.
    /// Call from any code path that changes something user-visible.
    pub(crate) fn request_redraw(&mut self) {
        self.needs_redraw = true;
    }

    /// Pre-draw state maintenance, run by the main loop right before a
    /// frame is drawn (not inside the render path): keep the cursor in
    /// view, refresh the syntax cache after edits, and push pending buffer
    /// changes to the LSP server. `content_rows` is the editor pane height
    /// (terminal height minus statusline + cmdline).
    pub(crate) fn update(&mut self, content_rows: usize) {
        let rows = content_rows.max(1);
        self.ensure_cursor_visible(rows);
        // A fullscreen picker covers the editor pane entirely — skip the
        // highlight refresh while it's up; the first frame after it closes
        // pays it instead.
        let covered = self
            .picker
            .as_ref()
            .is_some_and(|p| matches!(p.kind.spec().layout, crate::picker::PickerLayout::Full));
        if !covered {
            self.refresh_syntax_cache_for_viewport(self.view_top, rows);
        }
        self.sync_lsp_changes();
    }

    /// Clear an expired yank flash. Returns via `request_redraw` so the
    /// frame that erases the highlight actually gets drawn.
    pub(crate) fn decay_yank_flash(&mut self) {
        if let Some((_, until)) = self.yank_flash.as_ref() {
            if std::time::Instant::now() >= *until {
                self.yank_flash = None;
                self.request_redraw();
            }
        }
    }

    /// The earliest wall-clock instant at which time-driven state needs a
    /// wake-up: yank-flash expiry, the picker's query-debounce deadline, or
    /// the preview-rebuild debounce. Derived from state each loop iteration
    /// (never stored) so a missed set can't strand the screen stale.
    pub(crate) fn next_wake_deadline(&self) -> Option<std::time::Instant> {
        use std::time::Duration;
        let mut next: Option<std::time::Instant> = None;
        let mut consider = |t: std::time::Instant| {
            next = Some(match next {
                Some(cur) if cur <= t => cur,
                _ => t,
            });
        };
        if let Some((_, until)) = self.yank_flash.as_ref() {
            consider(*until);
        }
        if let Some(p) = self.picker.as_ref() {
            if let Some(dirty_at) = p.query_dirty_at {
                consider(dirty_at + Duration::from_millis(PICKER_REFRESH_DEBOUNCE_MS));
            }
        }
        // A preview rebuild owed but held by its debounce needs a wake to run.
        if let Some(t) = crate::picker::preview::preview_wake_deadline(self) {
            consider(t);
        }
        next
    }

    /// Whether any wakerless channel (LSP events, streaming scan/grep)
    /// might deliver data that only a poll tick can pick up. Governs the
    /// idle poll timeout: short while channels are live, long otherwise.
    pub(crate) fn has_live_channels(&self) -> bool {
        !self.lsp_clients.is_empty() || self.files_source.is_some() || self.grep_source.is_some()
    }

    /// Refresh `syntax_cache` for the given viewport (first visible line +
    /// row count). Uses the windowed query path when the language supports
    /// it: the cache covers the viewport plus a one-screen margin on each
    /// side, so line-by-line scrolling hits the cache, a bigger jump pays a
    /// windowed *query* (no reparse while the buffer is unchanged), and
    /// only an edit pays a parse — whose cost no longer includes running
    /// the highlights query over the whole file.
    pub(crate) fn refresh_syntax_cache_for_viewport(&mut self, first_line: usize, rows: usize) {
        let version = self.buffer.version();
        let total_lines = self.buffer.len_lines();
        let needed = self.line_span_bytes(first_line, first_line + rows, total_lines);
        if self.syntax_version == Some(version) {
            match &self.syntax_window {
                None => return, // whole-file cache is always valid
                Some(w) if w.start <= needed.start && needed.end <= w.end => return,
                _ => {}
            }
        }
        if self.syntax.is_none() {
            self.syntax_cache = Vec::new();
            self.syntax_window = None;
            self.syntax_version = Some(version);
            return;
        }

        // Rebuild the contiguous source copy only when the buffer actually
        // changed (or a different buffer became active).
        let src_key = (self.active_bid, version);
        if self.syntax_src_key != Some(src_key) {
            self.syntax_src.clear();
            for chunk in self.buffer.rope().chunks() {
                self.syntax_src.push_str(chunk);
            }
            self.syntax_src_key = Some(src_key);
        }

        // The retained parse tree inside SyntaxState is stale iff the cache
        // version lags — SyntaxState swaps with its buffer, so version
        // tracking here speaks for the tree too.
        let source_changed = self.syntax_version != Some(version);
        let margined = self.line_span_bytes(
            first_line.saturating_sub(rows),
            first_line + 2 * rows,
            total_lines,
        );
        let syn = self.syntax.as_mut().expect("checked above");
        match syn.highlight_range(self.syntax_src.as_bytes(), margined.clone(), source_changed) {
            Some(spans) => {
                self.syntax_cache = spans;
                self.syntax_window = Some(margined);
            }
            None => {
                // Locals-dependent language (or parse failure): full path.
                self.syntax_cache = syn
                    .highlight(self.syntax_src.as_bytes())
                    .unwrap_or_default();
                self.syntax_window = None;
            }
        }
        self.syntax_version = Some(version);
    }

    /// Refresh `syntax_cache` for the whole file (no windowing). The
    /// semantic baseline the viewport variant is tested against; only test
    /// code calls it (the editor always goes through the viewport path).
    #[cfg(test)]
    pub(crate) fn refresh_syntax_cache(&mut self) {
        let version = self.buffer.version();
        if self.syntax_version == Some(version) && self.syntax_window.is_none() {
            return;
        }
        self.syntax_cache = if let Some(s) = self.syntax.as_mut() {
            let src = self.buffer.rope().to_string();
            s.highlight(src.as_bytes()).unwrap_or_default()
        } else {
            Vec::new()
        };
        self.syntax_window = None;
        self.syntax_version = Some(version);
    }

    /// Byte range spanned by lines `[start_line, end_line)`, both clamped
    /// to the buffer. `end_line >= total` extends to the end of the buffer
    /// (a viewport can run past EOF).
    fn line_span_bytes(
        &self,
        start_line: usize,
        end_line: usize,
        total_lines: usize,
    ) -> std::ops::Range<usize> {
        let rope = self.buffer.rope();
        let start_line = start_line.min(total_lines.saturating_sub(1));
        let start = rope.char_to_byte(self.buffer.line_to_char(start_line));
        let end = if end_line >= total_lines {
            rope.len_bytes()
        } else {
            rope.char_to_byte(self.buffer.line_to_char(end_line))
        };
        start..end.max(start)
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
        // `rect` is the list area (no header inside it); every clickable row
        // maps straight to a list row.
        self.last_picker_rect = Some(rect);
        self.last_picker_scroll = scroll;
        self.last_picker_list_rows = rect.height as usize;
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::Harness;
    use std::time::{Duration, Instant};

    #[test]
    fn fresh_editor_has_no_wake_deadline_and_wants_first_frame() {
        let h = Harness::with_text("hello\n");
        assert!(h.editor.needs_redraw, "first frame must draw");
        assert!(h.editor.next_wake_deadline().is_none());
    }

    #[test]
    fn yank_flash_schedules_a_wake_and_decays_dirty() {
        let mut h = Harness::with_text("hello world\n");
        h.keys("yy");
        assert!(
            h.editor.yank_flash.is_some(),
            "yy should set the yank flash"
        );
        let wake = h
            .editor
            .next_wake_deadline()
            .expect("flash expiry must schedule a wake");
        assert!(wake > Instant::now() - Duration::from_millis(1));
        // Force-expire the flash: decay must clear it and mark the screen
        // dirty so the erase frame is drawn.
        h.editor.yank_flash = Some((0..1, Instant::now() - Duration::from_millis(1)));
        h.editor.needs_redraw = false;
        h.editor.decay_yank_flash();
        assert!(h.editor.yank_flash.is_none());
        assert!(h.editor.needs_redraw, "decay must request a redraw");
        assert!(h.editor.next_wake_deadline().is_none());
    }

    #[test]
    fn pending_picker_query_schedules_a_wake() {
        let mut h = Harness::with_text("hello\n");
        h.cmd("Buffers");
        assert!(h.picker_open());
        h.editor.needs_redraw = false;
        h.keys("x"); // types into the query -> marks it dirty (debounced)
        assert!(
            h.editor.next_wake_deadline().is_some(),
            "debounced query flush needs a timer wake"
        );
    }

    #[test]
    fn live_streaming_sources_shorten_the_idle_poll() {
        let mut h = Harness::with_text("hello\n");
        assert!(!h.editor.has_live_channels());
        h.keys("<Space>f");
        assert!(
            h.editor.has_live_channels(),
            "in-flight file scan must keep polling short"
        );
        h.pump_picker();
        assert!(!h.editor.has_live_channels());
    }

    /// Per-byte scope resolution over a byte window, from a flat span list.
    fn scopes_in(spans: &[vix_syntax::HlSpan], win: std::ops::Range<usize>) -> Vec<Option<usize>> {
        let mut v = vec![None; win.end - win.start];
        for s in spans {
            let a = s.range.start.max(win.start);
            let b = s.range.end.min(win.end);
            for i in a..b {
                v[i - win.start] = Some(s.scope);
            }
        }
        v
    }

    fn rust_fixture_harness() -> Harness {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/sample.rs"
        ))
        .unwrap();
        Harness::with_text_and_path(&src, "sample.rs")
    }

    #[test]
    fn viewport_syntax_cache_matches_full_highlight() {
        let mut h = rust_fixture_harness();
        // Baseline: whole-file highlight.
        h.editor.refresh_syntax_cache();
        let full = h.editor.syntax_cache.clone();
        assert!(h.editor.syntax_window.is_none());

        // Viewport path over several windows, including past-EOF clamps.
        let total = h.editor.buffer.len_lines();
        for (first, rows) in [(0usize, 10usize), (5, 8), (total.saturating_sub(3), 10)] {
            h.editor.invalidate_syntax_cache();
            h.editor.refresh_syntax_cache_for_viewport(first, rows);
            let win = h.editor.syntax_window.clone().expect("windowed path");
            assert_eq!(
                scopes_in(&h.editor.syntax_cache, win.clone()),
                scopes_in(&full, win),
                "viewport ({first},{rows}) diverged from full highlight"
            );
        }
    }

    #[test]
    fn viewport_syntax_cache_hits_and_invalidates() {
        let mut h = rust_fixture_harness();
        h.editor.refresh_syntax_cache_for_viewport(0, 10);
        let win = h.editor.syntax_window.clone().unwrap();
        // Same viewport again: pure cache hit, window unchanged.
        h.editor.refresh_syntax_cache_for_viewport(2, 8);
        assert_eq!(h.editor.syntax_window.clone().unwrap(), win);
        // An edit invalidates: the version bump forces a re-highlight.
        h.keys("ix<Esc>");
        h.editor.refresh_syntax_cache_for_viewport(0, 10);
        assert_eq!(
            h.editor.syntax_version,
            Some(h.editor.buffer.version()),
            "edit must re-key the cache"
        );
    }
}
