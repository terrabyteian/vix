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
use std::time::{Duration, Instant};

use vix_core::{
    apply_motion, compile_search, find_all_in_lines, find_backward, find_forward,
    handle_normal_char, text_object_range, Action, Buffer, Case, Change, FindDirection, FindKind,
    History, InsertPos, JumpEntry, JumpList, Mode, Motion, NormalKeyState, PendingOp, RepeatAction,
    SearchDirection, Selection, TextObject, TextObjectKind, Transaction,
};

pub mod help;
pub mod testing;
use vix_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, GotoDefinitionResponse, Hover, HoverContents, Location, MarkedString, Uri};
use vix_lsp::{parse_response, path_to_uri, server_for_path, uri_to_path, LspClient, RequestId, ServerEvent};
use vix_syntax::{HlSpan, Language, SyntaxState, HIGHLIGHT_NAMES, Symbol};
use vix_picker::{grep, scan_files, score, GrepItem, Utf32String};

/// What action triggered the current Insert session — determines how `.`
/// will replay it on Esc.
#[derive(Debug, Clone)]
enum InsertOrigin {
    /// `i/a/I/A/o/O` — bare insert mode entry.
    Plain,
    /// `c<motion>` — replay re-evaluates the motion at the cursor.
    ChangeMotion { motion: Motion, count: usize },
    /// `c<text-object>` — replay re-resolves the text object at the cursor.
    ChangeObject { object: TextObject, kind: TextObjectKind },
    /// `cc` (or `Ncc`) — replay deletes that many lines' content in place.
    ChangeLine { count: usize },
}

/// Accumulates text typed during an Insert-mode session, plus how that session
/// was entered. On Esc we commit this as one undo unit and one `.` repeat.
struct PendingInsert {
    pos: InsertPos,
    tx: Transaction,
    typed: String,
    /// Cursor position where insert mode began (post-positioning for a/A/o/O).
    /// Stored so `.` can reproduce the relative position.
    #[allow(dead_code)]
    start: usize,
    /// What action started this insert session.
    origin: InsertOrigin,
}

/// Contents of the unnamed register (`"`), plus whether the last yank/delete
/// was linewise — determines how `p`/`P` paste.
#[derive(Debug, Clone, Default)]
struct Register {
    text: String,
    linewise: bool,
}

/// Snapshot of everything per-buffer: used to park inactive buffers while
/// another is active. Switching buffers swaps one of these with the fields
/// living directly on `Editor`.
struct BufferSave {
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
    keys: NormalKeyState,
    history: History,
    pending_insert: Option<PendingInsert>,
    last_change: Option<RepeatAction>,
    /// Non-active buffers. Switching swaps the active set with one of these.
    other_buffers: Vec<BufferSave>,
    register: Register,
    /// Top line of the viewport (for vertical scrolling).
    pub view_top: usize,
    /// Command-line input buffer (active in Command and Search modes).
    pub cmdline: String,
    /// ':' for ex, '/' for forward search, '?' for backward.
    pub cmdline_prompt: char,
    /// Last search query (compiled). None when no search has been run.
    last_search: Option<(String, SearchDirection)>,
    /// Whether to render match highlights (cleared by :noh).
    hl_search: bool,
    /// Last char-find for `;`/`,` repeat.
    last_find: Option<(char, FindDirection, FindKind)>,
    /// Short status message shown at the right of the statusline.
    pub msg: String,
    pub quit: bool,
    /// Syntax highlighter, set if we recognized the file's language.
    syntax: Option<SyntaxState>,
    /// Cached highlight spans. Refreshed only when `syntax_version` lags
    /// behind `buffer.version()` — avoids reparsing on pure navigation.
    syntax_cache: Vec<HlSpan>,
    /// Buffer version the cache was computed against. `None` forces a rebuild
    /// on first use.
    syntax_version: Option<u64>,
    /// Active picker overlay (file finder / grep). Intercepts input while set.
    picker: Option<Picker>,
    /// Registered LSP clients keyed by `cmd`. We spawn lazily — one client per
    /// language per editor lifetime — and route per-buffer requests based on
    /// the file's extension.
    lsp_clients: HashMap<String, LspClient>,
    /// Per-buffer LSP document state: URI, monotonic version, latest
    /// buffer-version we sent to the server (to decide when to `didChange`).
    lsp_docs: HashMap<PathBuf, LspDocState>,
    /// Diagnostics per buffer path.
    diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    /// In-flight LSP request bookkeeping — maps server cmd + id → intent.
    /// Intent carries the buffer path for correlation after response arrives.
    pending_requests: HashMap<(String, RequestId), PendingRequest>,
    /// Server cmds we've already tried to spawn and failed on. Prevents
    /// re-spawning a missing binary on every K/gd.
    lsp_failed: std::collections::HashSet<String>,
    /// Per-server timestamps of recent crashes — keyed by server cmd. Used to
    /// rate-limit auto-restart (max 3 restarts per 60s before giving up).
    lsp_restart_log: HashMap<String, Vec<Instant>>,
    /// Bottom-area hover popup, set after a hover response arrives. Cleared
    /// by any subsequent keypress in Normal mode.
    hover_popup: Option<String>,
    /// Active completion popup in Insert mode, if any.
    completion_popup: Option<CompletionPopup>,
    /// Pending code actions awaiting user selection. Cleared when the picker
    /// closes.
    pending_code_actions: Vec<vix_lsp::lsp_types::CodeActionOrCommand>,
    /// Transient flash overlay after a yank — (range, expires_at).
    yank_flash: Option<(std::ops::Range<usize>, Instant)>,
    /// Jump-list ring for `Ctrl-O` / `Ctrl-I`. Entries are keyed by path + line
    /// + col so they survive buffer-index reshuffles and edits.
    jumps: JumpList,
    /// In Visual mode, the pending text-object kind from the last `i` / `a`.
    /// Cleared once the object char arrives or Esc is pressed.
    visual_object_kind: Option<TextObjectKind>,
    /// Stable id of the currently-active buffer. Paired with `BufferSave::bid`
    /// to render a position counter that tracks the active buffer through
    /// `<Tab>` / `:bn` rotations.
    active_bid: u64,
    /// Monotonic source for new buffer ids.
    next_bid: u64,
    /// Last rendered content rect. Used to translate mouse coords to buffer
    /// positions. None until the first frame is drawn.
    last_content_rect: Option<Rect>,
    /// Width of the gutter (line numbers + diag glyph + space) at the last
    /// render. Click x − content_rect.x − this = column into the line.
    last_gutter_cols: u16,
    /// Last rendered picker overlay rect, and the scroll offset into the
    /// match list at that frame. Used to translate mouse events on the picker
    /// back into list-item indices. `None` when no picker is up.
    last_picker_rect: Option<Rect>,
    last_picker_scroll: usize,
    /// Set true after `<Space>` is pressed in Normal mode. The next key
    /// resolves the leader sequence. Cleared on Esc / mode changes / Ctrl-C.
    pending_leader: bool,
    /// One-shot flag used at launch: when the user opens vix without a file
    /// (or with a directory), we boot with an empty placeholder buffer and
    /// pop the file picker. The first buffer they pick should *replace*
    /// that placeholder rather than park it. Consumed on the first swap.
    discard_active_on_swap: bool,
}

/// What we asked for — lets us interpret the response when it arrives.
#[derive(Clone, Debug)]
enum PendingRequest {
    Hover,
    Definition,
    /// Completion request. `prefix_start` is the char offset where the
    /// identifier under the cursor began when we sent the request, so we
    /// know what range to replace on accept.
    Completion { prefix_start: usize },
}

/// A pending completion popup in Insert mode.
#[derive(Clone, Debug, Default)]
struct CompletionPopup {
    /// Full list from the server.
    items: Vec<vix_lsp::lsp_types::CompletionItem>,
    /// Indices into `items` that match the current prefix (case-insensitive).
    visible: Vec<usize>,
    /// Cursor within `visible`.
    selected: usize,
    /// Char offset of the identifier's first char.
    prefix_start: usize,
}

/// Per-document LSP state. We stamp documents with a separate version number
/// from the buffer's mutation counter so we only emit `didChange` on real
/// text edits.
#[derive(Clone)]
struct LspDocState {
    uri: Uri,
    /// The LSP-visible document version. Starts at 1 on `didOpen`, bumped
    /// on every `didChange`.
    version: i32,
    /// Snapshot of `Buffer::version()` at the last `didChange`. Used to
    /// gate re-sync — we only push changes when this lags.
    last_sent_buffer_version: u64,
    /// Which server cmd owns this doc.
    server_cmd: String,
}

/// Overlay state for the file / grep pickers. The overlay owns input and
/// rendering while it's alive; dismissal returns control to Normal mode.
struct Picker {
    kind: PickerKind,
    query: String,
    /// `(display, value, haystack)` tuples. `value` is the selected payload
    /// (path or grep hit) passed to `on_select`.
    items: Vec<PickerItem>,
    /// Scored subset of `items` visible in the current list, plus the index
    /// back into `items`.
    matches: Vec<(usize, u32)>,
    selected: usize,
    /// Vertical scroll offset within the match list.
    scroll: usize,
    /// Cached file-scan items so `<Tab>` Files↔Grep toggling doesn't
    /// rescan the tree on every flip. Populated for the unified
    /// Files/Grep picker; left `None` for other picker kinds.
    cached_files: Option<Vec<PickerItem>>,
}

#[derive(Clone, Debug)]
enum PickerKind {
    Files,
    Grep,
    Symbols,
    Buffers,
    CodeActions,
    Jumps,
}

#[derive(Clone)]
struct PickerItem {
    display: String,
    value: PickerValue,
    haystack: Utf32String,
}

/// Selection payload. `File` is the selected path; `GrepHit` carries the
/// file + line number so we can jump after load.
#[derive(Clone, Debug)]
enum PickerValue {
    File(std::path::PathBuf),
    GrepHit { path: std::path::PathBuf, line: u64 },
    /// Char offset within the current buffer to jump the cursor to.
    BufferOffset(usize),
    /// Buffer index (0 = active, 1.. = parked) to switch to.
    BufferIndex(usize),
    /// Index into `Editor::pending_code_actions`.
    CodeAction(usize),
    /// Index into the jump list's current entries (oldest = 0).
    JumpIndex(usize),
}

/// Scan `cwd` for files (respecting `.gitignore`) and wrap them as picker
/// items. Used by the unified Files/Grep picker.
fn scan_files_as_picker_items(cwd: &std::path::Path) -> Vec<PickerItem> {
    scan_files(cwd)
        .into_iter()
        .map(|fi| PickerItem {
            display: fi.rel_path.to_string_lossy().into_owned(),
            value: PickerValue::File(fi.rel_path),
            haystack: fi.haystack,
        })
        .collect()
}

/// Run a regex grep across `cwd` and wrap results as picker items. Errors
/// (e.g. user typed an in-progress regex like `[`) collapse to an empty
/// list — no UX-disrupting noise during live typing.
fn grep_as_picker_items(cwd: &std::path::Path, query: &str) -> Vec<PickerItem> {
    let hits: Vec<GrepItem> = grep(cwd, query).unwrap_or_default();
    hits.into_iter()
        .map(|g| {
            let display = format!("{}:{}: {}", g.path.display(), g.line, g.text);
            PickerItem {
                display,
                value: PickerValue::GrepHit { path: g.path, line: g.line },
                haystack: g.haystack,
            }
        })
        .collect()
}

/// Render label for the buffer picker: "[1] path   [+]".
fn label_for_buffer(buf: &Buffer, idx: usize, active: bool) -> String {
    let tag = if active { "%" } else { " " };
    let name = buf
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "[No Name]".into());
    let dirty = if buf.dirty() { " [+]" } else { "" };
    format!("[{:>2}] {}  {}{}", idx + 1, tag, name, dirty)
}

impl Picker {
    /// Re-score items against `self.query`. Caps visible matches at 1000 to
    /// keep the render loop snappy on large repos.
    fn rescore(&mut self) {
        let scored: Vec<(usize, Utf32String)> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| (i, it.haystack.clone()))
            .collect();
        self.matches = score(&scored, &self.query, 1000);
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
        self.scroll = 0;
    }
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
            pending_leader: false,
            discard_active_on_swap: false,
            active_bid: 0,
            next_bid: 1,
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
    pub fn picker_open(&self) -> bool {
        self.picker.is_some()
    }
    pub fn picker_query(&self) -> Option<&str> {
        self.picker.as_ref().map(|p| p.query.as_str())
    }
    /// One-word label for the active picker kind (test introspection).
    #[doc(hidden)]
    pub fn picker_selected_for_test(&self) -> usize {
        self.picker.as_ref().map(|p| p.selected).unwrap_or(0)
    }
    pub fn picker_kind_label(&self) -> Option<&'static str> {
        self.picker.as_ref().map(|p| match p.kind {
            PickerKind::Files => "files",
            PickerKind::Grep => "grep",
            PickerKind::Symbols => "symbols",
            PickerKind::Buffers => "buffers",
            PickerKind::CodeActions => "code_actions",
            PickerKind::Jumps => "jumps",
        })
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
        let Some(s) = self.syntax.as_ref() else { return Vec::new() };
        let src = self.buffer.rope().to_string();
        s.symbols(src.as_bytes())
            .ok()
            .map(|v| v.into_iter().map(|sym| sym.name).collect())
            .unwrap_or_default()
    }

    /// Ensure an LSP server is running for the active buffer's language, and
    /// that the buffer is open on it. Idempotent per (server, path).
    fn ensure_lsp_open(&mut self) {
        let Some(path) = self.buffer.path() else { return };
        let path: PathBuf = path.to_path_buf();
        let Some(config) = server_for_path(&path) else { return };
        // Spawn the server if we haven't already.
        let cmd = config.cmd.clone();
        if self.lsp_failed.contains(&cmd) {
            return;
        }
        if !self.lsp_clients.contains_key(&cmd) {
            let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            match LspClient::start(config, &root) {
                Ok(c) => {
                    self.lsp_clients.insert(cmd.clone(), c);
                }
                Err(e) => {
                    self.msg = format!("lsp: {e}");
                    self.lsp_failed.insert(cmd);
                    return;
                }
            }
        }
        // Open the document if we haven't already. `didChange` batching handles
        // subsequent edits via `sync_lsp_changes`.
        if self.lsp_docs.contains_key(&path) {
            return;
        }
        let Ok(uri) = path_to_uri(&path) else {
            self.msg = "lsp: bad path".into();
            return;
        };
        let text = self.buffer.rope().to_string();
        if let Some(client) = self.lsp_clients.get(&cmd) {
            client.did_open(uri.clone(), 1, text);
        }
        self.lsp_docs.insert(
            path,
            LspDocState {
                uri,
                version: 1,
                last_sent_buffer_version: self.buffer.version(),
                server_cmd: cmd,
            },
        );
    }

    /// If the active buffer's LSP doc version lags the buffer's mutation
    /// counter, send a full `didChange` and bump. Full-text sync is
    /// heavier than incremental but dead-simple; incremental can come later.
    fn sync_lsp_changes(&mut self) {
        let Some(path) = self.buffer.path() else { return };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get_mut(&path) else { return };
        let bv = self.buffer.version();
        if doc.last_sent_buffer_version == bv {
            return;
        }
        doc.version += 1;
        doc.last_sent_buffer_version = bv;
        let uri = doc.uri.clone();
        let version = doc.version;
        let cmd = doc.server_cmd.clone();
        let text = self.buffer.rope().to_string();
        if let Some(client) = self.lsp_clients.get(&cmd) {
            client.did_change_full(uri, version, text);
        }
    }

    /// Drain any pending events from all running LSP clients.
    fn drain_lsp_events(&mut self) {
        // Collect first (separate borrows) then dispatch.
        let mut batch: Vec<(String, ServerEvent)> = Vec::new();
        for (cmd, client) in &self.lsp_clients {
            while let Some(ev) = client.try_recv() {
                batch.push((cmd.clone(), ev));
            }
        }
        for (cmd, ev) in batch {
            self.handle_lsp_event(cmd, ev);
        }
    }

    fn handle_lsp_event(&mut self, cmd: String, ev: ServerEvent) {
        match ev {
            ServerEvent::Diagnostics { uri, diagnostics } => {
                if let Some(path) = uri_to_path(&uri) {
                    if diagnostics.is_empty() {
                        self.diagnostics.remove(&path);
                    } else {
                        self.diagnostics.insert(path, diagnostics);
                    }
                }
            }
            ServerEvent::Response { id, result, error } => {
                if let Some(intent) = self.pending_requests.remove(&(cmd, id)) {
                    self.handle_lsp_response(intent, result, error);
                }
            }
            ServerEvent::Log { level: _, message: _ } => {
                // Stash the most recent server log for debugging; drop in msg
                // only if nothing more useful is present.
            }
            ServerEvent::Show { level: _, message } => {
                self.msg = message;
            }
            ServerEvent::Exited => {
                // Tear down the dead client and drop its doc state; the next
                // ensure_lsp_open will respawn if we're still under budget.
                self.lsp_clients.remove(&cmd);
                self.lsp_docs.retain(|_, doc| doc.server_cmd != cmd);
                self.pending_requests.retain(|(c, _), _| c != &cmd);
                let now = Instant::now();
                let entry = self.lsp_restart_log.entry(cmd.clone()).or_default();
                entry.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
                if entry.len() >= 3 {
                    self.lsp_failed.insert(cmd.clone());
                    self.msg = format!("lsp {cmd}: crashed 3x in 60s — giving up");
                } else {
                    entry.push(now);
                    self.msg = format!("lsp {cmd}: crashed, will restart on next edit");
                }
            }
        }
    }

    fn handle_lsp_response(
        &mut self,
        intent: PendingRequest,
        result: Option<vix_lsp::Value>,
        error: Option<String>,
    ) {
        if let Some(e) = error {
            self.msg = format!("lsp: {e}");
            return;
        }
        match intent {
            PendingRequest::Hover => match parse_response::<Hover>(result) {
                Ok(Some(h)) => {
                    let text = hover_text(&h);
                    if text.trim().is_empty() {
                        self.msg = "no hover info".into();
                    } else {
                        self.hover_popup = Some(text);
                    }
                }
                Ok(None) => self.msg = "no hover info".into(),
                Err(e) => self.msg = format!("lsp hover: {e}"),
            },
            PendingRequest::Definition => match parse_response::<GotoDefinitionResponse>(result) {
                Ok(Some(resp)) => self.jump_to_definition(resp),
                Ok(None) => self.msg = "no definition".into(),
                Err(e) => self.msg = format!("lsp definition: {e}"),
            },
            PendingRequest::Completion { prefix_start } => {
                match parse_response::<vix_lsp::lsp_types::CompletionResponse>(result) {
                    Ok(Some(resp)) => {
                        use vix_lsp::lsp_types::CompletionResponse;
                        let items = match resp {
                            CompletionResponse::Array(v) => v,
                            CompletionResponse::List(l) => l.items,
                        };
                        if items.is_empty() {
                            self.completion_popup = None;
                            self.msg = "no completions".into();
                        } else {
                            // Only install the popup if we're still in Insert
                            // and the cursor hasn't moved before prefix_start.
                            if self.mode == Mode::Insert && self.sel.head >= prefix_start {
                                self.completion_popup = Some(CompletionPopup {
                                    items,
                                    visible: Vec::new(),
                                    selected: 0,
                                    prefix_start,
                                });
                                self.refilter_completions();
                            }
                        }
                    }
                    Ok(None) => {
                        self.completion_popup = None;
                        self.msg = "no completions".into();
                    }
                    Err(e) => self.msg = format!("lsp completion: {e}"),
                }
            }
        }
    }

    fn jump_to_definition(&mut self, resp: GotoDefinitionResponse) {
        let loc: Option<Location> = match resp {
            GotoDefinitionResponse::Scalar(l) => Some(l),
            GotoDefinitionResponse::Array(mut xs) => xs.drain(..).next(),
            GotoDefinitionResponse::Link(mut xs) => xs.drain(..).next().map(|l| Location {
                uri: l.target_uri,
                range: l.target_selection_range,
            }),
        };
        let Some(loc) = loc else {
            self.msg = "no definition".into();
            return;
        };
        let Some(path) = uri_to_path(&loc.uri) else {
            self.msg = "lsp: non-file target".into();
            return;
        };
        // Record the departure even if we're jumping within the same buffer,
        // so `Ctrl-O` returns to the call site after `gd`.
        let same_buf = self.buffer.path().map(|p| p == path.as_path()).unwrap_or(false);
        if same_buf {
            self.push_jump();
        }
        // `open_path` already records departure when switching buffers.
        self.open_path(&path);
        let line = loc.range.start.line as usize;
        let ch = loc.range.start.character as usize;
        let line = line.min(self.buffer.len_lines().saturating_sub(1));
        let line_start = self.buffer.line_to_char(line);
        let line_len = self.buffer.line_len_chars(line);
        let offset = line_start + ch.min(line_len);
        self.sel = Selection::at(offset).clamped(&self.buffer);
    }

    /// Send a hover request at the current cursor. No-op if the buffer isn't
    /// bound to an LSP server.
    fn request_hover(&mut self) {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else { return };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else {
            self.msg = "lsp: no server".into();
            return;
        };
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        if let Some(client) = self.lsp_clients.get(&doc.server_cmd) {
            let id = client.hover(doc.uri, line as u32, col as u32);
            self.pending_requests
                .insert((doc.server_cmd, id), PendingRequest::Hover);
        }
    }

    /// Send a goto-definition request at the current cursor.
    fn request_definition(&mut self) {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else { return };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else {
            self.msg = "lsp: no server".into();
            return;
        };
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        if let Some(client) = self.lsp_clients.get(&doc.server_cmd) {
            let id = client.definition(doc.uri, line as u32, col as u32);
            self.pending_requests
                .insert((doc.server_cmd, id), PendingRequest::Definition);
        }
    }

    /// Send a completion request from the current cursor. Records the word
    /// prefix start so we know what range to replace when the user accepts.
    fn request_completion(&mut self) {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else { return };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else {
            self.msg = "lsp: no server".into();
            return;
        };
        let prefix_start = self.word_prefix_start(self.sel.head);
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        if let Some(client) = self.lsp_clients.get(&doc.server_cmd) {
            let id = client.completion(doc.uri, line as u32, col as u32);
            self.pending_requests.insert(
                (doc.server_cmd, id),
                PendingRequest::Completion { prefix_start },
            );
        }
    }

    /// Walk back from `at` over identifier chars (alphanumeric or `_`) to find
    /// the start of the word under the cursor.
    fn word_prefix_start(&self, at: usize) -> usize {
        let rope = self.buffer.rope();
        let mut i = at;
        while i > 0 {
            let c = rope.char(i - 1);
            if c.is_alphanumeric() || c == '_' {
                i -= 1;
            } else {
                break;
            }
        }
        i
    }

    /// Rebuild `visible` based on the current prefix (chars between
    /// `prefix_start` and the cursor), case-insensitive prefix match.
    fn refilter_completions(&mut self) {
        let Some(popup) = self.completion_popup.as_mut() else { return };
        if self.sel.head < popup.prefix_start {
            self.completion_popup = None;
            return;
        }
        let prefix: String = self
            .buffer
            .rope()
            .slice(popup.prefix_start..self.sel.head)
            .to_string();
        let prefix_lc = prefix.to_lowercase();
        popup.visible.clear();
        for (idx, item) in popup.items.iter().enumerate() {
            let hay = item
                .filter_text
                .as_deref()
                .unwrap_or(item.label.as_str());
            if hay.to_lowercase().starts_with(&prefix_lc) {
                popup.visible.push(idx);
            }
        }
        if popup.visible.is_empty() {
            self.completion_popup = None;
        } else {
            popup.selected = popup.selected.min(popup.visible.len() - 1);
        }
    }

    /// Apply the currently-selected completion item: replace the prefix range
    /// with the item's text.
    fn accept_completion(&mut self) {
        let Some(popup) = self.completion_popup.take() else { return };
        let Some(&item_idx) = popup.visible.get(popup.selected) else { return };
        let item = &popup.items[item_idx];
        let insert_text = item
            .insert_text
            .clone()
            .unwrap_or_else(|| item.label.clone());
        // Replace [prefix_start, cursor) with insert_text via the pending
        // insert session so undo groups this with the rest of the insert.
        let end = self.sel.head;
        let start = popup.prefix_start.min(end);
        // Delete backwards from cursor to prefix_start
        for _ in start..end {
            self.backspace_in_session();
        }
        for c in insert_text.chars() {
            self.insert_char_in_session(c);
        }
    }

    /// Select the next / previous completion item. Wraps.
    fn move_completion_selection(&mut self, delta: isize) {
        let Some(popup) = self.completion_popup.as_mut() else { return };
        if popup.visible.is_empty() { return; }
        let len = popup.visible.len() as isize;
        let cur = popup.selected as isize;
        let new = ((cur + delta) % len + len) % len;
        popup.selected = new as usize;
    }

    /// Request code actions at the current cursor + present a picker. On
    /// accept the chosen action is applied (edit and/or command).
    fn run_code_action(&mut self) {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else { self.msg = "no file".into(); return };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else { self.msg = "lsp: no server".into(); return };
        let Some(client) = self.lsp_clients.get(&doc.server_cmd) else { return };
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        // Range: if we're in a Visual selection, use it; otherwise a
        // zero-width range at the cursor. Diagnostics on the cursor's line
        // are passed as context so the server knows what to suggest.
        let range = match self.mode {
            Mode::Visual | Mode::VisualLine => {
                let r = self.visual_range();
                let (sl, sc) = self.buffer.char_to_line_col(r.start);
                let (el, ec) = self.buffer.char_to_line_col(r.end);
                vix_lsp::lsp_types::Range {
                    start: vix_lsp::lsp_types::Position { line: sl as u32, character: sc as u32 },
                    end: vix_lsp::lsp_types::Position { line: el as u32, character: ec as u32 },
                }
            }
            _ => vix_lsp::lsp_types::Range {
                start: vix_lsp::lsp_types::Position { line: line as u32, character: col as u32 },
                end: vix_lsp::lsp_types::Position { line: line as u32, character: col as u32 },
            },
        };
        let diags: Vec<_> = self
            .diagnostics
            .get(&path)
            .map(|v| {
                v.iter()
                    .filter(|d| {
                        let s = d.range.start.line as usize;
                        let e = d.range.end.line as usize;
                        s <= line && line <= e
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let id = client.code_action(doc.uri, range, diags);
        let result = match client.wait_response(id, Duration::from_millis(3000)) {
            Some((res, None)) => res,
            Some((_, Some(e))) => { self.msg = format!("code action: {e}"); return; }
            None => { self.msg = "code action: timed out".into(); return; }
        };
        let Some(result) = result else {
            self.msg = "no code actions".into();
            return;
        };
        let actions: Vec<vix_lsp::lsp_types::CodeActionOrCommand> =
            match parse_response(Some(result)) {
                Ok(Some(a)) => a,
                _ => { self.msg = "no code actions".into(); return; }
            };
        if actions.is_empty() {
            self.msg = "no code actions".into();
            return;
        }
        // Build picker items. Value is the index into `pending_code_actions`.
        let mut items: Vec<PickerItem> = Vec::with_capacity(actions.len());
        for (i, act) in actions.iter().enumerate() {
            let title = match act {
                vix_lsp::lsp_types::CodeActionOrCommand::Command(c) => c.title.clone(),
                vix_lsp::lsp_types::CodeActionOrCommand::CodeAction(a) => a.title.clone(),
            };
            let haystack = Utf32String::from(title.as_str());
            items.push(PickerItem {
                display: title,
                value: PickerValue::CodeAction(i),
                haystack,
            });
        }
        self.pending_code_actions = actions;
        let mut p = Picker {
            kind: PickerKind::CodeActions,
            query: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            cached_files: None,
        };
        p.rescore();
        self.picker = Some(p);
    }

    /// Apply a selected code action: first its WorkspaceEdit (if any), then
    /// its Command (best-effort — we log unknown commands rather than round-
    /// tripping `workspace/executeCommand`).
    fn apply_code_action(&mut self, idx: usize) {
        let Some(action) = self.pending_code_actions.get(idx).cloned() else { return };
        match action {
            vix_lsp::lsp_types::CodeActionOrCommand::Command(cmd) => {
                self.run_lsp_command(cmd);
            }
            vix_lsp::lsp_types::CodeActionOrCommand::CodeAction(a) => {
                if let Some(edit) = a.edit {
                    self.apply_workspace_edit(edit);
                }
                if let Some(cmd) = a.command {
                    self.run_lsp_command(cmd);
                }
            }
        }
        self.pending_code_actions.clear();
    }

    /// Send `workspace/executeCommand`. Fire-and-forget; most rust-analyzer
    /// commands just bounce back a WorkspaceEdit via `workspace/applyEdit`,
    /// which we don't currently handle — but the edit part of most actions
    /// is already returned inline in the CodeAction and applied above.
    fn run_lsp_command(&mut self, cmd: vix_lsp::lsp_types::Command) {
        let Some(path) = self.buffer.path() else { return };
        let Some(doc) = self.lsp_docs.get(path).cloned() else { return };
        let Some(client) = self.lsp_clients.get(&doc.server_cmd) else { return };
        let _ = client.execute_command(cmd.command, cmd.arguments.unwrap_or_default());
    }

    /// Send `textDocument/rename` at the cursor, wait for the server's
    /// WorkspaceEdit, and apply it across all affected files. Files already
    /// open (active or parked) are edited in-place; files only on disk are
    /// loaded, edited, and written back.
    fn run_rename(&mut self, new_name: &str) {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else { self.msg = "lsp: no file".into(); return };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else { self.msg = "lsp: no server".into(); return };
        let Some(client) = self.lsp_clients.get(&doc.server_cmd) else { return };
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        let id = client.rename(doc.uri.clone(), line as u32, col as u32, new_name.to_string());
        let result = match client.wait_response(id, Duration::from_millis(5000)) {
            Some((res, None)) => res,
            Some((_, Some(e))) => { self.msg = format!("rename: {e}"); return; }
            None => { self.msg = "rename: timed out (server still indexing?)".into(); return; }
        };
        let Some(result) = result else {
            self.msg = "rename: not renamable at this position".into();
            return;
        };
        let edit: vix_lsp::lsp_types::WorkspaceEdit =
            match parse_response(Some(result.clone())) {
                Ok(Some(e)) => e,
                Ok(None) => { self.msg = "rename: not renamable at this position".into(); return; }
                Err(e) => { self.msg = format!("rename: bad response: {e}"); return; }
            };
        self.apply_workspace_edit(edit);
    }

    /// Apply a WorkspaceEdit across active, parked, and on-disk files.
    /// Servers may send either the legacy `changes` map or the newer
    /// `documentChanges` list — we flatten both to `(Uri, Vec<TextEdit>)`.
    fn apply_workspace_edit(&mut self, edit: vix_lsp::lsp_types::WorkspaceEdit) {
        use vix_lsp::lsp_types::{DocumentChangeOperation, DocumentChanges, OneOf};

        let mut per_file: Vec<(vix_lsp::lsp_types::Uri, Vec<vix_lsp::lsp_types::TextEdit>)> = Vec::new();
        if let Some(changes) = edit.changes {
            for (uri, edits) in changes {
                per_file.push((uri, edits));
            }
        }
        if let Some(doc_changes) = edit.document_changes {
            match doc_changes {
                DocumentChanges::Edits(edits) => {
                    for tde in edits {
                        let plain: Vec<_> = tde
                            .edits
                            .into_iter()
                            .map(|e| match e {
                                OneOf::Left(te) => te,
                                OneOf::Right(ann) => ann.text_edit,
                            })
                            .collect();
                        per_file.push((tde.text_document.uri, plain));
                    }
                }
                DocumentChanges::Operations(ops) => {
                    for op in ops {
                        if let DocumentChangeOperation::Edit(tde) = op {
                            let plain: Vec<_> = tde
                                .edits
                                .into_iter()
                                .map(|e| match e {
                                    OneOf::Left(te) => te,
                                    OneOf::Right(ann) => ann.text_edit,
                                })
                                .collect();
                            per_file.push((tde.text_document.uri, plain));
                        }
                    }
                }
            }
        }

        if per_file.is_empty() {
            self.msg = "rename: server returned no edits".into();
            return;
        }

        let mut files_touched = 0usize;
        let mut total_edits = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for (uri, edits) in per_file {
            let Some(path) = uri_to_path(&uri) else {
                errors.push(format!("non-file uri: {}", uri.as_str()));
                continue;
            };
            total_edits += edits.len();
            if self.apply_edits_to_any_buffer(&path, &edits, &mut errors) {
                files_touched += 1;
            }
        }
        let mut msg = format!("renamed in {files_touched} file(s), {total_edits} edit(s)");
        if !errors.is_empty() {
            msg.push_str(" — errors: ");
            msg.push_str(&errors.join("; "));
        }
        self.msg = msg;
    }

    /// Apply `edits` to the buffer at `path`. If it's the active buffer, edit
    /// in place. If parked, edit the parked copy. If not loaded, read from
    /// disk, apply, and write back.
    fn apply_edits_to_any_buffer(
        &mut self,
        path: &std::path::Path,
        edits: &[vix_lsp::lsp_types::TextEdit],
        errors: &mut Vec<String>,
    ) -> bool {
        if self.buffer.path() == Some(path) {
            self.apply_text_edits(edits);
            return true;
        }
        if let Some(pos) = self
            .other_buffers
            .iter()
            .position(|b| b.buffer.path() == Some(path))
        {
            let save = &mut self.other_buffers[pos];
            let mut tx = Transaction::new();
            tx.sel_before = Some(save.sel);
            apply_text_edits_to_buffer_tx(&mut save.buffer, edits, &mut tx);
            save.sel = save.sel.clamped(&save.buffer);
            tx.sel_after = Some(save.sel);
            if !tx.is_empty() {
                save.history.commit(tx);
            }
            return true;
        }
        // Not loaded: read, edit, write. No history to commit to — the
        // Transaction is built and discarded.
        match Buffer::load(path) {
            Ok(mut buf) => {
                let mut throwaway = Transaction::new();
                apply_text_edits_to_buffer_tx(&mut buf, edits, &mut throwaway);
                if let Err(e) = buf.save() {
                    errors.push(format!("write {}: {e}", path.display()));
                    return false;
                }
                true
            }
            Err(e) => {
                errors.push(format!("read {}: {e}", path.display()));
                false
            }
        }
    }

    /// Request `textDocument/formatting`, apply returned edits to the active
    /// buffer. Blocks the UI for up to ~1.5s. No-op if no LSP is attached.
    fn format_buffer(&mut self) -> bool {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else { return false };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else { return false };
        let Some(client) = self.lsp_clients.get(&doc.server_cmd) else { return false };
        let id = client.formatting(doc.uri.clone(), 4, true);
        match client.wait_response(id, Duration::from_millis(1500)) {
            Some((Some(result), None)) => {
                match parse_response::<Vec<vix_lsp::lsp_types::TextEdit>>(Some(result)) {
                    Ok(Some(edits)) => {
                        self.apply_text_edits(&edits);
                        true
                    }
                    Ok(None) => false,
                    Err(e) => { self.msg = format!("lsp format: {e}"); false }
                }
            }
            Some((_, Some(err))) => { self.msg = format!("lsp format: {err}"); false }
            Some((None, None)) => false,
            None => { self.msg = "lsp: format timed out".into(); false }
        }
    }

    /// Format (if LSP is attached) and write to disk.
    fn format_and_save(&mut self) {
        self.format_buffer();
        match self.buffer.save() {
            Ok(()) => self.msg = format!(
                "\"{}\" written",
                self.buffer.path().map(|p| p.display().to_string()).unwrap_or_default()
            ),
            Err(e) => self.msg = format!("error: {e}"),
        }
    }

    /// Apply a slice of LSP `TextEdit`s to the active buffer. Edits are
    /// applied bottom-up (by start position) so earlier offsets stay valid.
    /// Note: `character` is treated as a char index, not UTF-16 code units —
    /// fine for the all-ASCII source files we typically format.
    ///
    /// The whole batch is recorded as a single `Transaction` committed to
    /// `self.history` — so `u` undoes the entire LSP edit in one step. We
    /// deliberately do NOT touch `self.last_change`: LSP edits must not
    /// pollute `.` repeat (Vim's rule; see plan doc trap #2).
    pub fn apply_text_edits(&mut self, edits: &[vix_lsp::lsp_types::TextEdit]) {
        if edits.is_empty() { return; }
        let sel_before = self.sel;
        let mut tx = Transaction::new();
        tx.sel_before = Some(sel_before);
        apply_text_edits_to_buffer_tx(&mut self.buffer, edits, &mut tx);
        // Cursor may now be past EOF; clamp.
        self.sel = self.sel.clamped(&self.buffer);
        tx.sel_after = Some(self.sel);
        if !tx.is_empty() {
            self.history.commit(tx);
        }
    }

    /// Open the file finder picker rooted at the current working directory.
    pub fn open_files_picker(&mut self) {
        self.open_picker_unified(PickerKind::Files, "");
    }

    /// Unified Files↔Grep picker. `<Tab>` toggles submode, query carries
    /// over. Files results are scanned once on open and cached so toggling
    /// is free.
    fn open_picker_unified(&mut self, initial: PickerKind, initial_query: &str) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let cached_files = scan_files_as_picker_items(&cwd);
        let items = match initial {
            PickerKind::Files => cached_files.clone(),
            PickerKind::Grep => {
                if initial_query.len() >= 2 {
                    grep_as_picker_items(&cwd, initial_query)
                } else {
                    Vec::new()
                }
            }
            _ => return,
        };
        let mut p = Picker {
            kind: initial,
            query: initial_query.to_string(),
            items,
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            cached_files: Some(cached_files),
        };
        p.rescore();
        self.picker = Some(p);
    }

    /// Toggle the active picker between Files and Grep submodes. The query
    /// is preserved; the items list is regenerated from the cache (Files)
    /// or by re-running grep (Grep, if query is at least 2 chars).
    fn toggle_picker_mode(&mut self) {
        let (new_kind, query, cached) = {
            let Some(p) = self.picker.as_mut() else { return };
            let new_kind = match p.kind {
                PickerKind::Files => PickerKind::Grep,
                PickerKind::Grep => PickerKind::Files,
                _ => return,
            };
            (new_kind, p.query.clone(), p.cached_files.clone())
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let new_items = match new_kind {
            PickerKind::Grep => {
                if query.len() >= 2 {
                    grep_as_picker_items(&cwd, &query)
                } else {
                    Vec::new()
                }
            }
            PickerKind::Files => match cached {
                Some(c) => c,
                None => scan_files_as_picker_items(&cwd),
            },
            _ => return,
        };
        let Some(p) = self.picker.as_mut() else { return };
        p.kind = new_kind;
        p.items = new_items;
        if matches!(p.kind, PickerKind::Files) && p.cached_files.is_none() {
            p.cached_files = Some(p.items.clone());
        }
        p.selected = 0;
        p.scroll = 0;
        p.rescore();
    }

    /// Re-grep on each query change in Grep submode. Requires ≥2 chars;
    /// shorter queries clear the items list.
    fn refresh_grep_items(&mut self) {
        let query = match self.picker.as_ref() {
            Some(p) if matches!(p.kind, PickerKind::Grep) => p.query.clone(),
            _ => return,
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let new_items = if query.len() >= 2 {
            grep_as_picker_items(&cwd, &query)
        } else {
            Vec::new()
        };
        let Some(p) = self.picker.as_mut() else { return };
        p.items = new_items;
        p.rescore();
    }

    /// Open the tree-sitter symbol picker for the current buffer.
    fn open_symbols_picker(&mut self) {
        let Some(s) = self.syntax.as_ref() else {
            self.msg = "no language bound".into();
            return;
        };
        let src = self.buffer.rope().to_string();
        let symbols: Vec<Symbol> = s.symbols(src.as_bytes()).unwrap_or_else(|e| {
            self.msg = format!("symbols: {e}");
            Vec::new()
        });
        if symbols.is_empty() && self.msg.is_empty() {
            self.msg = "no symbols found".into();
            return;
        }
        // Map byte offset → char offset (tree-sitter works in bytes; we jump
        // with char offsets).
        let rope = self.buffer.rope();
        let items: Vec<PickerItem> = symbols
            .into_iter()
            .map(|sym| {
                let char_off = rope.byte_to_char(sym.start_byte);
                let (line, _) = self.buffer.char_to_line_col(char_off);
                let display = format!("{:<8} {}  L{}", sym.kind, sym.name, line + 1);
                let haystack = Utf32String::from(sym.name.as_str());
                PickerItem {
                    display,
                    value: PickerValue::BufferOffset(char_off),
                    haystack,
                }
            })
            .collect();
        let mut p = Picker {
            kind: PickerKind::Symbols,
            query: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            cached_files: None,
        };
        p.rescore();
        self.picker = Some(p);
    }

    /// Open the grep picker, optionally pre-filled with `pattern`. Pattern
    /// becomes the initial query; the live grep machinery handles results.
    fn open_grep_picker(&mut self, pattern: &str) {
        self.open_picker_unified(PickerKind::Grep, pattern);
    }

    /// Handle a key event while the picker overlay is active. Returns true if
    /// the event was consumed; false means the picker closed itself.
    fn handle_picker_key(&mut self, k: KeyEvent) -> bool {
        // What to do *after* picker-internal mutation completes. Lets us
        // release the &mut borrow before calling self-mutating helpers.
        enum Post {
            None,
            Close,
            Select(PickerValue),
            Toggle,
            // Re-score (Files) or re-grep (Grep) after the query changed.
            Refresh,
        }

        let post = {
            let Some(p) = self.picker.as_mut() else { return false };
            let is_unified = matches!(p.kind, PickerKind::Files | PickerKind::Grep);
            match k.code {
                KeyCode::Esc => {
                    if is_unified && !p.query.is_empty() {
                        p.query.clear();
                        Post::Refresh
                    } else {
                        Post::Close
                    }
                }
                KeyCode::Tab => {
                    if is_unified {
                        Post::Toggle
                    } else {
                        Post::None
                    }
                }
                KeyCode::Enter => {
                    if let Some(&(idx, _)) = p.matches.get(p.selected) {
                        Post::Select(p.items[idx].value.clone())
                    } else {
                        Post::Close
                    }
                }
                KeyCode::Up => {
                    if p.selected > 0 {
                        p.selected -= 1;
                    }
                    Post::None
                }
                KeyCode::Down => {
                    if p.selected + 1 < p.matches.len() {
                        p.selected += 1;
                    }
                    Post::None
                }
                KeyCode::Backspace => {
                    if p.query.pop().is_some() {
                        Post::Refresh
                    } else {
                        Post::None
                    }
                }
                KeyCode::Char(c) if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    match c {
                        'n' | 'j' => {
                            if p.selected + 1 < p.matches.len() {
                                p.selected += 1;
                            }
                        }
                        'p' | 'k' => {
                            if p.selected > 0 {
                                p.selected -= 1;
                            }
                        }
                        _ => {}
                    }
                    Post::None
                }
                KeyCode::Char(c) => {
                    p.query.push(c);
                    Post::Refresh
                }
                _ => Post::None,
            }
        };

        match post {
            Post::None => {}
            Post::Close => {
                self.picker = None;
            }
            Post::Select(v) => {
                self.picker = None;
                self.pick_result(v);
            }
            Post::Toggle => {
                self.toggle_picker_mode();
            }
            Post::Refresh => {
                let is_grep = matches!(
                    self.picker.as_ref().map(|p| &p.kind),
                    Some(PickerKind::Grep)
                );
                if is_grep {
                    self.refresh_grep_items();
                } else if let Some(p) = self.picker.as_mut() {
                    p.rescore();
                }
            }
        }
        true
    }

    /// Handle a mouse event while the picker overlay is active. Scroll
    /// moves the selection (so the visible window follows automatically);
    /// left-click on a row activates that entry. Clicks outside the overlay
    /// or on the header row are ignored.
    fn handle_picker_mouse(&mut self, me: MouseEvent) {
        let selected_value: Option<PickerValue> = {
            let Some(p) = self.picker.as_mut() else { return };
            match me.kind {
                MouseEventKind::ScrollUp => {
                    if p.selected > 0 {
                        p.selected -= 1;
                    }
                    None
                }
                MouseEventKind::ScrollDown => {
                    if p.selected + 1 < p.matches.len() {
                        p.selected += 1;
                    }
                    None
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    let Some(rect) = self.last_picker_rect else { return };
                    if me.column < rect.x
                        || me.row < rect.y
                        || me.column >= rect.x + rect.width
                        || me.row >= rect.y + rect.height
                    {
                        return;
                    }
                    let row_in_overlay = me.row - rect.y;
                    // Row 0 is the header; list rows start at 1.
                    if row_in_overlay == 0 {
                        return;
                    }
                    let list_row = (row_in_overlay - 1) as usize;
                    let match_idx = self.last_picker_scroll + list_row;
                    if let Some(&(item_idx, _)) = p.matches.get(match_idx) {
                        p.selected = match_idx;
                        Some(p.items[item_idx].value.clone())
                    } else {
                        return;
                    }
                }
                _ => return,
            }
        };
        if let Some(v) = selected_value {
            self.picker = None;
            self.pick_result(v);
        }
    }

    /// Act on a picker selection: open a file or jump to a grep hit.
    fn pick_result(&mut self, value: PickerValue) {
        match value {
            PickerValue::File(path) => self.open_path(&path),
            PickerValue::GrepHit { path, line } => {
                self.open_path(&path);
                // Jump to the hit line (1-based).
                let target = line.saturating_sub(1) as usize;
                let target = target.min(self.buffer.len_lines().saturating_sub(1));
                let ch = self.buffer.line_to_char(target);
                self.sel = Selection::at(ch).clamped(&self.buffer);
            }
            PickerValue::BufferOffset(ch) => {
                self.push_jump();
                self.sel = Selection::at(ch).clamped(&self.buffer);
            }
            PickerValue::BufferIndex(idx) => {
                if idx != 0 {
                    self.push_jump();
                }
                self.switch_to_buffer(idx);
            }
            PickerValue::CodeAction(idx) => {
                self.apply_code_action(idx);
            }
            PickerValue::JumpIndex(idx) => {
                self.apply_jump_pick(idx);
            }
        }
    }

    /// Load `path` as a new buffer (or switch to it if already open). The
    /// previous active buffer is parked, including if it has unsaved edits
    /// — Vim-style `hidden`.
    /// Open a help topic in a scratch buffer. `topic` may be empty to show
    /// the index page. Subsequent `:help <same>` calls switch back to the
    /// existing buffer instead of duplicating it (path-keyed dedup).
    fn open_help_doc(&mut self, topic: &str) {
        let topic = topic.trim();
        let (slug, body) = if topic.is_empty() {
            ("index".to_string(), help::index())
        } else if let Some(t) = help::lookup(topic) {
            (t.slug.to_string(), t.body.to_string())
        } else {
            self.msg = format!(
                "no help topic \"{topic}\" — try :help for the index"
            );
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

    fn open_path(&mut self, path: &std::path::Path) {
        // Record departure on any switch / load — but not when the target is
        // already the active buffer.
        let same_as_active = self
            .buffer
            .path()
            .map(|p| p == path)
            .unwrap_or(false);
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
                    self.buffer.path().map(|p| p.display().to_string()).unwrap_or_default()
                );
            }
            Err(e) => self.msg = format!("error: {e}"),
        }
    }

    /// Refresh `syntax_cache` if the buffer has mutated since the last parse.
    /// Cheap fast path when the user is just navigating (no edits).
    fn refresh_syntax_cache(&mut self) {
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
    fn invalidate_syntax_cache(&mut self) {
        self.syntax_version = None;
        self.syntax_cache.clear();
    }

    /// Snapshot the currently-active buffer for parking. Leaves placeholder
    /// defaults behind (caller is expected to immediately install a new
    /// active buffer on top).
    fn save_active(&mut self) -> BufferSave {
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
    fn install_active(&mut self, save: BufferSave) {
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
    fn assign_new_active_bid(&mut self) {
        self.active_bid = self.next_bid;
        self.next_bid = self.next_bid.wrapping_add(1).max(1);
    }

    /// Replace the active buffer with a freshly-loaded one. Parks the
    /// currently-active buffer in `other_buffers` so the user can return to
    /// it. If a buffer with the same path is already open, switch to it
    /// rather than loading twice.
    fn add_or_switch_buffer(&mut self, buffer: Buffer) {
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
    fn buffer_index_by_path(&self, path: &std::path::Path) -> Option<usize> {
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
    fn switch_to_buffer(&mut self, idx: usize) {
        if idx == 0 || idx >= self.buffer_count() {
            return;
        }
        let promoted = self.other_buffers.remove(idx - 1);
        let current = self.save_active();
        self.install_active(promoted);
        self.other_buffers.push(current);
    }

    /// `:bn` — cycle to the next buffer (wraps). No-op with a single buffer.
    fn next_buffer(&mut self) {
        if self.other_buffers.is_empty() {
            self.msg = "E86: Only one buffer".into();
            return;
        }
        self.push_jump();
        // The "next" buffer is conceptually the oldest parked one (FIFO).
        self.switch_to_buffer(1);
    }

    /// `:bp` — cycle to the previous buffer. Symmetric to `:bn`.
    fn prev_buffer(&mut self) {
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
    fn close_buffer(&mut self, force: bool) {
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
    fn any_buffer_dirty(&self) -> bool {
        self.buffer.dirty() || self.other_buffers.iter().any(|b| b.buffer.dirty())
    }

    /// Capture the current cursor position as a jump-list entry.
    fn current_jump_entry(&self) -> JumpEntry {
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
    fn push_jump(&mut self) {
        self.jumps.push(self.current_jump_entry());
    }

    /// Move the active buffer + cursor to the entry. If the target lives in a
    /// different buffer (or an on-disk file not currently open), we switch or
    /// load it. Returns false if the buffer couldn't be located or loaded.
    fn goto_jump_entry(&mut self, entry: JumpEntry) -> bool {
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
    fn jump_back(&mut self) {
        let current = self.current_jump_entry();
        match self.jumps.back(current) {
            Some(e) => {
                if !self.goto_jump_entry(e) { /* msg set in callee */ }
            }
            None => self.msg = "at top of jump list".into(),
        }
    }

    /// `Ctrl-I` / Tab — step forward.
    fn jump_forward(&mut self) {
        match self.jumps.forward() {
            Some(e) => {
                if !self.goto_jump_entry(e) { /* msg set in callee */ }
            }
            None => self.msg = "at bottom of jump list".into(),
        }
    }

    /// `:b <spec>` — switch to buffer matching `spec`. Numeric = 1-based
    /// index; non-numeric = substring match over buffer paths.
    fn switch_buffer_by_spec(&mut self, spec: &str) {
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

    /// `:jumps` — open a picker listing jump-list entries. Selection jumps to
    /// that entry. Informational column shows `>` next to the current position.
    fn open_jumps_picker(&mut self) {
        if self.jumps.is_empty() {
            self.msg = "jump list is empty".into();
            return;
        }
        let pos = self.jumps.pos();
        let entries: Vec<(usize, JumpEntry)> =
            self.jumps.entries().cloned().enumerate().collect();
        let mut items: Vec<PickerItem> = Vec::with_capacity(entries.len());
        for (i, e) in entries.iter() {
            let marker = if *i == pos { '>' } else { ' ' };
            let path = e
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "[No Name]".into());
            let label = format!("{}  {:>3}  L{}:{}  {}", marker, i + 1, e.line + 1, e.col + 1, path);
            items.push(PickerItem {
                display: label.clone(),
                value: PickerValue::JumpIndex(*i),
                haystack: Utf32String::from(label.as_str()),
            });
        }
        let mut p = Picker {
            kind: PickerKind::Jumps,
            query: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            cached_files: None,
        };
        p.rescore();
        self.picker = Some(p);
    }

    /// Consume a `:jumps`-picker selection: jump to `entries[idx]` and update
    /// the internal `pos` so subsequent Ctrl-O/I walk from there. We walk the
    /// jump list by calling `back`/`forward` until pos matches — cheap enough
    /// for the 100-entry cap.
    fn apply_jump_pick(&mut self, idx: usize) {
        let cur = self.jumps.pos();
        if idx == cur {
            return;
        }
        if idx < cur {
            for _ in idx..cur {
                let c = self.current_jump_entry();
                if let Some(e) = self.jumps.back(c) {
                    if !self.goto_jump_entry(e) {
                        return;
                    }
                }
            }
        } else {
            for _ in cur..idx {
                if let Some(e) = self.jumps.forward() {
                    if !self.goto_jump_entry(e) {
                        return;
                    }
                }
            }
        }
    }

    /// Open the buffer picker: lists all live buffers for fuzzy selection.
    fn open_buffers_picker(&mut self) {
        let mut items: Vec<PickerItem> = Vec::with_capacity(self.buffer_count());
        let active_label = label_for_buffer(&self.buffer, 0, true);
        items.push(PickerItem {
            display: active_label.clone(),
            value: PickerValue::BufferIndex(0),
            haystack: Utf32String::from(active_label.as_str()),
        });
        for (i, b) in self.other_buffers.iter().enumerate() {
            let label = label_for_buffer(&b.buffer, i + 1, false);
            items.push(PickerItem {
                display: label.clone(),
                value: PickerValue::BufferIndex(i + 1),
                haystack: Utf32String::from(label.as_str()),
            });
        }
        let mut p = Picker {
            kind: PickerKind::Buffers,
            query: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            cached_files: None,
        };
        p.rescore();
        self.picker = Some(p);
    }

    fn do_search(&mut self, query: &str, dir: SearchDirection) {
        if query.is_empty() { return; }
        let re = match compile_search(query, Case::Smart) {
            Ok(r) => r,
            Err(e) => { self.msg = format!("E: {e}"); return; }
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
            }
            None => { self.msg = format!("E486: Pattern not found: {query}"); }
        }
    }

    fn word_search_under(&mut self, dir: SearchDirection) {
        let rope = self.buffer.rope();
        let len = self.buffer.len_chars();
        if len == 0 { return; }
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let mut start = self.sel.head.min(len.saturating_sub(1));
        // If cursor is not on a word char, try to find one to the right on this line.
        if !is_word(rope.char(start)) {
            let (line, _) = self.buffer.char_to_line_col(start);
            let line_end = self.buffer.line_to_char(line) + self.buffer.line_len_chars(line);
            let mut i = start;
            while i < line_end && !is_word(rope.char(i)) { i += 1; }
            if i >= line_end { self.msg = "E348: No string under cursor".into(); return; }
            start = i;
        }
        // Extend backward to start of word.
        while start > 0 && is_word(rope.char(start - 1)) { start -= 1; }
        let mut end = start;
        while end < len && is_word(rope.char(end)) { end += 1; }
        let word: String = rope.slice(start..end).to_string();
        // Build a pattern with word boundaries, escaping regex metachars.
        let escaped = regex_escape_like(&word);
        let pattern = format!(r"\b{escaped}\b");
        self.do_search(&pattern, dir);
    }

    fn search_repeat(&mut self, dir: SearchDirection) {
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

    fn cursor_line(&self) -> usize { self.buffer.char_to_line_col(self.sel.head).0 }

    /// Char-range covered by the current Visual/VisualLine selection, ready
    /// to be consumed by an operator.
    fn visual_range(&self) -> std::ops::Range<usize> {
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
                if end < self.buffer.len_chars() { start..(end + 1) } else { start..end }
            }
            _ => r,
        }
    }

    fn ensure_cursor_visible(&mut self, viewport_rows: usize) {
        let line = self.cursor_line();
        if line < self.view_top {
            self.view_top = line;
        } else if line >= self.view_top + viewport_rows {
            self.view_top = line + 1 - viewport_rows;
        }
    }

    fn run_ex(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        match cmd {
            "w" => self.format_and_save(),
            "fmt" | "format" => { self.format_buffer(); }
            "action" | "actions" | "ca" => { self.run_code_action(); }
            "q" => {
                if self.buffer.dirty() {
                    self.msg = "E37: No write since last change (use :q!)".into();
                } else {
                    self.close_buffer(false);
                }
            }
            "q!" => { self.close_buffer(true); }
            "qa" | "qall" => {
                if self.any_buffer_dirty() {
                    self.msg = "E37: unsaved buffers exist (use :qa!)".into();
                } else {
                    self.quit = true;
                }
            }
            "qa!" | "qall!" => { self.quit = true; }
            "wq" | "x" => {
                self.format_and_save();
                if !self.buffer.dirty() {
                    self.close_buffer(false);
                }
            }
            "noh" | "nohl" | "nohlsearch" => { self.hl_search = false; }
            "Files" => { self.open_files_picker(); }
            "Symbols" => { self.open_symbols_picker(); }
            "Buffers" | "ls" => { self.open_buffers_picker(); }
            "jumps" => { self.open_jumps_picker(); }
            "help" | "h" => { self.open_help_doc(""); }
            "bn" | "bnext" => { self.next_buffer(); }
            "bp" | "bprev" | "bprevious" => { self.prev_buffer(); }
            "bd" | "bdelete" => { self.close_buffer(false); }
            "bd!" | "bdelete!" => { self.close_buffer(true); }
            "" => {}
            _ => {
                if let Some(rest) = cmd.strip_prefix("Grep") {
                    self.open_grep_picker(rest.trim());
                } else if let Some(rest) = cmd.strip_prefix("help ").or_else(|| cmd.strip_prefix("h ")) {
                    self.open_help_doc(rest.trim());
                } else if let Some(rest) = cmd.strip_prefix("e ") {
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
    fn run_substitute(&mut self, cmd: &str) {
        // Strip the range prefix + the `s`.
        let rest = if let Some(r) = cmd.strip_prefix("%s") { r }
                   else if let Some(r) = cmd.strip_prefix(".s") { r }
                   else if let Some(r) = cmd.strip_prefix('s') { r }
                   else { self.msg = "internal: bad :s".into(); return; };
        let whole_file = cmd.starts_with("%s");

        // First char after `s` is the delimiter.
        let mut chars = rest.chars();
        let Some(delim) = chars.next() else { self.msg = "E471: usage :s/pat/rep/flags".into(); return; };
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

        let re = match regex::RegexBuilder::new(pattern).case_insensitive(case_insensitive).build() {
            Ok(r) => r,
            Err(e) => { self.msg = format!("E: {e}"); return; }
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
                if !global { break; }
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
            tx.push(Change::Delete { at: range.start, removed: old.clone() });
            tx.push(Change::Insert { at: range.start, text: new_text.clone() });
        }
        self.sel = self.sel.clamped(&self.buffer);
        tx.sel_after = Some(self.sel);
        self.history.commit(tx);
        self.msg = format!("{count} substitutions");
    }

    /// Dispatch a resolved Action. Mutates editor state.
    fn dispatch(&mut self, action: Action) {
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
                    self.sel = Selection { anchor: self.sel.anchor, head: new_sel.head, virt_col: new_sel.virt_col };
                } else {
                    self.sel = new_sel;
                }
                self.sel = self.sel.clamped(&self.buffer);
            }
            Action::EnterMode(m) => {
                if m == Mode::Command { self.cmdline.clear(); }
                if matches!(m, Mode::Visual | Mode::VisualLine) {
                    self.sel.anchor = self.sel.head;
                }
                self.mode = m;
            }
            Action::EnterInsert(pos) => { self.enter_insert(pos); }
            Action::Operate(op, m, n) => {
                // `G` and `gg` with an operator behave linewise (vim parity).
                if matches!(m, Motion::BufferStart | Motion::BufferEnd) {
                    let cur_line = self.cursor_line();
                    let target_line = match m {
                        Motion::BufferStart => {
                            if n == 0 { 0 } else { n.saturating_sub(1).min(self.buffer.len_lines().saturating_sub(1)) }
                        }
                        Motion::BufferEnd => {
                            if n == 0 {
                                self.buffer.len_lines().saturating_sub(1)
                            } else {
                                n.saturating_sub(1).min(self.buffer.len_lines().saturating_sub(1))
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
                    let end_line_char = self.buffer.line_to_char(hi)
                        + self.buffer.line_len_chars(hi);
                    let end = if end_line_char < self.buffer.len_chars() {
                        end_line_char + 1
                    } else {
                        end_line_char
                    };
                    let entered_insert = self.apply_operator_with_kind(op, start..end, true);
                    if !entered_insert {
                        self.last_change = Some(RepeatAction::OperateLine { op, count: line_count });
                    }
                    return;
                }

                // `cw` / `cW` are vim-special: they act like `ce` / `cE`,
                // i.e. change to end-of-word without consuming the trailing
                // whitespace. We rewrite the motion before evaluating it.
                let m = if matches!(op, PendingOp::Change)
                    && matches!(m, Motion::WordForward)
                {
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
                        pi.origin = InsertOrigin::ChangeMotion { motion: m, count: n };
                    }
                } else {
                    self.last_change = Some(RepeatAction::Operate { op, motion: m, count: n });
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
                            op, object: obj, kind, count: 1,
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
            Action::RepeatLastChange => { self.repeat_last_change(); }
            Action::Paste { after, count } => {
                for _ in 0..count.max(1) { self.paste(after); }
                self.last_change = Some(RepeatAction::Paste { after, count });
            }
            Action::ExCommand(cmd) => { self.run_ex(&cmd); }
            Action::EnterSearch(dir) => {
                self.cmdline.clear();
                self.cmdline_prompt = match dir { SearchDirection::Forward => '/', SearchDirection::Backward => '?' };
                self.mode = Mode::Command;
            }
            Action::SearchRepeat(dir) => { self.push_jump(); self.search_repeat(dir); }
            Action::WordSearchUnder(dir) => { self.push_jump(); self.word_search_under(dir); }
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
                    match dir { FindDirection::Forward => FindDirection::Backward, FindDirection::Backward => FindDirection::Forward }
                } else { dir };
                self.sel = apply_motion(&self.buffer, self.sel, Motion::FindChar(c, effective_dir, kind), count).clamped(&self.buffer);
            }
            Action::LspHover => { self.request_hover(); }
            Action::LspGotoDefinition => { self.request_definition(); }
            Action::LspCodeAction => { self.run_code_action(); }
            Action::JumpBack => { self.jump_back(); }
            Action::JumpForward => { self.jump_forward(); }
            Action::Pending | Action::Unhandled => {}
        }
    }

    /// Position cursor for the given insert style and begin recording.
    fn enter_insert(&mut self, pos: InsertPos) {
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
                tx.push(Change::Insert { at: line_end, text: "\n".into() });
                self.sel = Selection::at(line_end + 1);
            }
            InsertPos::OpenAbove => {
                let line_start = self.buffer.line_to_char(line);
                self.buffer.insert_char(line_start, '\n');
                tx.push(Change::Insert { at: line_start, text: "\n".into() });
                self.sel = Selection::at(line_start);
            }
        }

        self.pending_insert = Some(PendingInsert {
            pos,
            tx,
            typed: String::new(),
            start: self.sel.head,
            origin: InsertOrigin::Plain,
        });
        self.mode = Mode::Insert;
    }

    /// Returns true if the operator entered Insert mode (c/cc).
    fn apply_operator(&mut self, op: PendingOp, range: std::ops::Range<usize>) -> bool {
        self.apply_operator_with_kind(op, range, false)
    }

    /// Linewise-aware operator application. `linewise` affects the register
    /// tag on yank/delete.
    fn apply_operator_with_kind(&mut self, op: PendingOp, range: std::ops::Range<usize>, linewise: bool) -> bool {
        match op {
            PendingOp::Delete => {
                let removed: String = self.buffer.rope().slice(range.clone()).to_string();
                self.register = Register { text: removed.clone(), linewise };
                let sel_before = self.sel;
                self.buffer.remove_range(range.clone());
                let sel_after = Selection::at(range.start).clamped(&self.buffer);
                self.sel = sel_after;
                let mut tx = Transaction::new();
                tx.sel_before = Some(sel_before);
                tx.push(Change::Delete { at: range.start, removed });
                tx.sel_after = Some(sel_after);
                self.history.commit(tx);
                false
            }
            PendingOp::Change => {
                let removed: String = self.buffer.rope().slice(range.clone()).to_string();
                self.register = Register { text: removed.clone(), linewise };
                let sel_before = self.sel;
                self.buffer.remove_range(range.clone());
                let sel_after = Selection::at(range.start).clamped(&self.buffer);
                self.sel = sel_after;
                let mut tx = Transaction::new();
                tx.sel_before = Some(sel_before);
                tx.push(Change::Delete { at: range.start, removed });
                self.pending_insert = Some(PendingInsert {
                    pos: InsertPos::AtCursor,
                    tx,
                    typed: String::new(),
                    start: self.sel.head,
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
                self.yank_flash = Some((
                    range.clone(),
                    Instant::now() + Duration::from_millis(150),
                ));
                // Yank doesn't move cursor in Normal, but in Visual it returns to
                // Normal mode with cursor at start of selection (Vim quirk).
                if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
                    self.sel = Selection::at(range.start).clamped(&self.buffer);
                }
                false
            }
            PendingOp::SwapCase => {
                let source: String = self.buffer.rope().slice(range.clone()).to_string();
                let replacement: String = source.chars().map(|c| {
                    if c.is_uppercase() { c.to_lowercase().next().unwrap_or(c) }
                    else if c.is_lowercase() { c.to_uppercase().next().unwrap_or(c) }
                    else { c }
                }).collect();
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
            _ => {
                self.msg = format!("{op:?} not yet implemented");
                false
            }
        }
    }

    /// Replace `range` (currently holding `old`) with `new_text` as one transaction.
    fn replace_range(&mut self, range: std::ops::Range<usize>, old: &str, new_text: &str) {
        let sel_before = self.sel;
        self.buffer.remove_range(range.clone());
        self.buffer.insert_str(range.start, new_text);
        let new_len = new_text.chars().count();
        self.sel = Selection::at(range.start + new_len.saturating_sub(1).max(0)).clamped(&self.buffer);
        let mut tx = Transaction::new();
        tx.sel_before = Some(sel_before);
        tx.push(Change::Delete { at: range.start, removed: old.to_string() });
        tx.push(Change::Insert { at: range.start, text: new_text.to_string() });
        tx.sel_after = Some(self.sel);
        self.history.commit(tx);
    }

    /// Indent or outdent the lines touched by `range`. 4 spaces per level.
    fn indent_range(&mut self, range: std::ops::Range<usize>, right: bool) {
        let (first_line, _) = self.buffer.char_to_line_col(range.start);
        let (mut last_line, _) = self.buffer.char_to_line_col(range.end.saturating_sub(1));
        if range.end == 0 { last_line = first_line; }
        let sel_before = self.sel;
        let mut tx = Transaction::new();
        tx.sel_before = Some(sel_before);

        // Operate bottom-up so earlier line offsets stay valid.
        for line in (first_line..=last_line).rev() {
            let start = self.buffer.line_to_char(line);
            if right {
                self.buffer.insert_str(start, "    ");
                tx.push(Change::Insert { at: start, text: "    ".into() });
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

    /// Paste the unnamed register after (`p`) or before (`P`) the cursor.
    fn paste(&mut self, after: bool) {
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
                self.buffer.line_to_char(line) + self.buffer.line_len_chars(line) + if line + 1 < self.buffer.len_lines() { 1 } else { 0 }
            } else {
                self.buffer.line_to_char(line)
            };
            // Ensure the pasted block ends with a newline for clean line boundary.
            let to_insert = if text.ends_with('\n') { text.clone() } else { format!("{text}\n") };
            self.buffer.insert_str(insert_at, &to_insert);
            tx.push(Change::Insert { at: insert_at, text: to_insert.clone() });
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
            tx.push(Change::Insert { at: insert_at, text: text.clone() });
            let n = text.chars().count();
            cursor_after = insert_at + n.saturating_sub(1).max(0);
        }

        self.sel = Selection::at(cursor_after).clamped(&self.buffer);
        tx.sel_after = Some(self.sel);
        self.history.commit(tx);
    }

    /// Re-dispatch the last change at the current cursor.
    fn repeat_last_change(&mut self) {
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
                let end = if end < self.buffer.len_chars() { end + 1 } else { end };
                self.apply_operator_with_kind(op, start..end, true);
            }
            RepeatAction::OperateObject { op, object, kind, count: _ } => {
                if let Some(range) = text_object_range(&self.buffer, self.sel.head, object, kind) {
                    self.apply_operator(op, range);
                }
            }
            RepeatAction::InsertBurst { pos, text } => {
                self.enter_insert(pos);
                for c in text.chars() { self.insert_char_in_session(c); }
                self.leave_insert();
            }
            RepeatAction::DeleteChars { forward, count } => {
                let range = self.delete_chars_range(forward, count);
                if range.start < range.end {
                    self.apply_operator(PendingOp::Delete, range);
                }
            }
            RepeatAction::ChangeMotion { motion, count, text } => {
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
                for c in text.chars() { self.insert_char_in_session(c); }
                self.leave_insert();
            }
            RepeatAction::ChangeObject { object, kind, text } => {
                if let Some(range) = text_object_range(&self.buffer, self.sel.head, object, kind) {
                    self.apply_operator(PendingOp::Change, range);
                    for c in text.chars() { self.insert_char_in_session(c); }
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
                for c in text.chars() { self.insert_char_in_session(c); }
                self.leave_insert();
            }
            RepeatAction::Paste { after, count } => {
                for _ in 0..count.max(1) { self.paste(after); }
            }
        }
    }

    /// Compute the char range for `x` / `X`: `count` chars forward or backward
    /// from the cursor, clamped to the current line (so x on the last char of
    /// a line deletes that char, and X at col 0 is a no-op).
    fn delete_chars_range(&self, forward: bool, count: usize) -> std::ops::Range<usize> {
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
    fn insert_char_in_session(&mut self, c: char) {
        let at = self.sel.head;
        self.buffer.insert_char(at, c);
        self.sel.head += 1;
        self.sel.anchor = self.sel.head;
        if let Some(pi) = self.pending_insert.as_mut() {
            let text = c.to_string();
            pi.tx.push(Change::Insert { at, text: text.clone() });
            pi.typed.push(c);
        }
    }

    /// Backspace in Insert mode. Records the deletion.
    fn backspace_in_session(&mut self) {
        if self.sel.head == 0 { return; }
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
    fn leave_insert(&mut self) {
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
        if col > 0 { self.sel.head -= 1; self.sel.anchor = self.sel.head; }
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
            if self.mode == Mode::Insert { self.leave_insert(); }
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
                        self.sel = apply_motion(&self.buffer, self.sel, motion, 1)
                            .clamped(&self.buffer);
                        if let Some(pi) = self.pending_insert.as_mut() {
                            pi.typed.clear();
                            pi.start = self.sel.head;
                        }
                        return;
                    }
                    Mode::Command => return,
                }
            }
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
                                'f' => { self.open_files_picker(); return; }
                                'g' => { self.open_grep_picker(""); return; }
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
                            if let Some(range) = text_object_range(&self.buffer, self.sel.head, o, kind) {
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
                    if c == 'i' { self.visual_object_kind = Some(TextObjectKind::Inner); return; }
                    if c == 'a' { self.visual_object_kind = Some(TextObjectKind::Around); return; }
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
                        if !entered_insert { self.mode = Mode::Normal; }
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
                    if c == 'v' && self.mode == Mode::VisualLine { self.mode = Mode::Visual; return; }
                    if c == 'V' && self.mode == Mode::Visual { self.mode = Mode::VisualLine; return; }
                    if c == 'v' && self.mode == Mode::Visual { self.mode = Mode::Normal; self.sel.anchor = self.sel.head; return; }
                    if c == 'V' && self.mode == Mode::VisualLine { self.mode = Mode::Normal; self.sel.anchor = self.sel.head; return; }
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
                let is_trigger = (ctrl && matches!(k.code, KeyCode::Char(' ')))
                    || k.code == KeyCode::Char('\0'); // some terminals send NUL for Ctrl-Space
                if is_trigger {
                    if popup_open { self.move_completion_selection(1); }
                    else { self.request_completion(); }
                    return;
                }
                if ctrl && matches!(k.code, KeyCode::Char('n')) {
                    if popup_open { self.move_completion_selection(1); }
                    else { self.request_completion(); }
                    return;
                }
                if ctrl && matches!(k.code, KeyCode::Char('p')) {
                    if popup_open { self.move_completion_selection(-1); }
                    else { self.request_completion(); }
                    return;
                }

                // Popup-only navigation / accept.
                if popup_open {
                    match k.code {
                        KeyCode::Down => { self.move_completion_selection(1); return; }
                        KeyCode::Up => { self.move_completion_selection(-1); return; }
                        KeyCode::Tab | KeyCode::Enter => { self.accept_completion(); return; }
                        KeyCode::Esc => { self.completion_popup = None; return; }
                        _ => {}
                    }
                }

                match k.code {
                    KeyCode::Esc => { self.leave_insert(); }
                    KeyCode::Char(c) => {
                        self.insert_char_in_session(c);
                        if self.completion_popup.is_some() { self.refilter_completions(); }
                    }
                    KeyCode::Enter => { self.insert_char_in_session('\n'); }
                    KeyCode::Backspace => {
                        self.backspace_in_session();
                        if self.completion_popup.is_some() { self.refilter_completions(); }
                    }
                    KeyCode::Tab => {
                        for _ in 0..4 { self.insert_char_in_session(' '); }
                    }
                    _ => {}
                }
            }
            Mode::Command => match k.code {
                KeyCode::Esc => { self.mode = Mode::Normal; self.cmdline.clear(); self.cmdline_prompt = ':'; }
                KeyCode::Enter => {
                    let cmd = std::mem::take(&mut self.cmdline);
                    let prompt = self.cmdline_prompt;
                    self.mode = Mode::Normal;
                    self.cmdline_prompt = ':';
                    match prompt {
                        '/' => { self.push_jump(); self.do_search(&cmd, SearchDirection::Forward); }
                        '?' => { self.push_jump(); self.do_search(&cmd, SearchDirection::Backward); }
                        _ => self.run_ex(&cmd),
                    }
                }
                KeyCode::Backspace => {
                    if self.cmdline.pop().is_none() {
                        self.mode = Mode::Normal;
                        self.cmdline_prompt = ':';
                    }
                }
                KeyCode::Char(c) => { self.cmdline.push(c); }
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
                self.sel = apply_motion(&self.buffer, self.sel, Motion::Up, 1)
                    .clamped(&self.buffer);
            }
            MouseEventKind::ScrollDown => {
                self.sel = apply_motion(&self.buffer, self.sel, Motion::Down, 1)
                    .clamped(&self.buffer);
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
    }

    /// Translate absolute terminal (col, row) to a buffer char offset, or
    /// None if the click was outside the content area / past EOF.
    fn click_to_char(&self, col: u16, row: u16) -> Option<usize> {
        let rect = self.last_content_rect?;
        if col < rect.x
            || row < rect.y
            || col >= rect.x + rect.width
            || row >= rect.y + rect.height
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

/// "E:2 W:1 " if the active buffer has diagnostics, else empty.
fn diag_summary(ed: &Editor) -> String {
    let Some(path) = ed.buffer.path() else { return String::new() };
    let Some(diags) = ed.diagnostics.get(path) else { return String::new() };
    let (mut e, mut w) = (0u32, 0u32);
    for d in diags {
        match d.severity.unwrap_or(DiagnosticSeverity::INFORMATION) {
            DiagnosticSeverity::ERROR => e += 1,
            DiagnosticSeverity::WARNING => w += 1,
            _ => {}
        }
    }
    if e == 0 && w == 0 {
        String::new()
    } else {
        format!("E:{e} W:{w} ")
    }
}

fn sev_rank(s: DiagnosticSeverity) -> u8 {
    match s {
        DiagnosticSeverity::ERROR => 4,
        DiagnosticSeverity::WARNING => 3,
        DiagnosticSeverity::INFORMATION => 2,
        DiagnosticSeverity::HINT => 1,
        _ => 0,
    }
}

/// Flatten a hover response to a plain string the TUI can render.
fn hover_text(h: &Hover) -> String {
    let piece = |s: &str| s.to_string();
    match &h.contents {
        HoverContents::Scalar(s) => match s {
            MarkedString::String(s) => piece(s),
            MarkedString::LanguageString(ls) => piece(&ls.value),
        },
        HoverContents::Array(arr) => arr
            .iter()
            .map(|m| match m {
                MarkedString::String(s) => s.clone(),
                MarkedString::LanguageString(ls) => ls.value.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(mc) => mc.value.clone(),
    }
}

/// Escape regex metacharacters for safe interpolation into a search pattern.
/// Apply a slice of LSP `TextEdit`s to an arbitrary buffer. Mirrors
/// `Editor::apply_text_edits` but works on buffers not owned by `Editor`
/// (used when applying rename edits to parked or on-disk buffers).
/// Apply LSP edits to `buf`, optionally recording each primitive edit into
/// `tx` (for history / undo). Pass `&mut Transaction::new()` and discard if
/// you don't want history tracking (e.g. when writing an on-disk file we're
/// not keeping open).
fn apply_text_edits_to_buffer_tx(
    buf: &mut Buffer,
    edits: &[vix_lsp::lsp_types::TextEdit],
    tx: &mut Transaction,
) {
    if edits.is_empty() { return; }
    let mut sorted: Vec<_> = edits.iter().collect();
    sorted.sort_by(|a, b| {
        let ak = (a.range.start.line, a.range.start.character);
        let bk = (b.range.start.line, b.range.start.character);
        bk.cmp(&ak)
    });
    for e in sorted {
        let start_line = (e.range.start.line as usize).min(buf.len_lines());
        let end_line = (e.range.end.line as usize).min(buf.len_lines());
        let start_char = buf.line_to_char(start_line)
            + (e.range.start.character as usize).min(buf.line_len_chars(start_line));
        let end_char = buf.line_to_char(end_line)
            + (e.range.end.character as usize).min(buf.line_len_chars(end_line));
        if start_char <= end_char && end_char <= buf.len_chars() {
            if start_char < end_char {
                let removed: String = buf.rope().slice(start_char..end_char).to_string();
                buf.remove_range(start_char..end_char);
                tx.push(Change::Delete { at: start_char, removed });
            }
            if !e.new_text.is_empty() {
                buf.insert_str(start_char, &e.new_text);
                tx.push(Change::Insert { at: start_char, text: e.new_text.clone() });
            }
        }
    }
}

/// Write `text` to the terminal's system clipboard via the OSC 52 escape
/// sequence. Works over SSH on any terminal that supports it (iTerm2,
/// WezTerm, kitty, Alacritty, Ghostty, tmux with set-clipboard on, etc.).
fn osc52_copy(text: &str) {
    use std::io::Write;
    // Skip pathologically large yanks. Most terminals cap OSC 52 around
    // 8-100KB; blasting a huge sequence can jam the terminal.
    if text.len() > 100_000 { return; }
    let b64 = base64_encode(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x07");
    let _ = out.flush();
}

/// Minimal RFC 4648 base64 encoder. Used for OSC 52 clipboard writes.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in chunks.by_ref() {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push_str("==");
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => unreachable!(),
    }
    out
}

fn regex_escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".+*?()[]{}|^$\\/".contains(c) { out.push('\\'); }
        out.push(c);
    }
    out
}

fn render(f: &mut ratatui::Frame, ed: &mut Editor) {
    let area = f.area();
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
        if Instant::now() >= *until { ed.yank_flash = None; }
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
    }
}

fn render_hover(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    let Some(text) = ed.hover_popup.as_deref() else { return };
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
    let h = (wrapped.len() as u16 + 2).min(area.height.saturating_sub(2)).max(3);
    let w = max_w;
    let x = area.x + area.width.saturating_sub(w) - 1;
    let y = area.y + 1;
    let rect = Rect::new(x, y, w, h);

    let bg = Style::default().bg(Color::DarkGray).fg(Color::White);
    let blank: Vec<Line> = (0..h).map(|_| Line::raw(" ".repeat(w as usize))).collect();
    f.render_widget(Paragraph::new(blank).style(bg), rect);

    let mut lines: Vec<Line> = Vec::with_capacity(h as usize);
    lines.push(Line::styled(" hover ".to_string() + &" ".repeat((w as usize).saturating_sub(7)),
        Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)));
    for l in wrapped.iter().take((h as usize).saturating_sub(2)) {
        let pad = (w as usize).saturating_sub(l.chars().count() + 1);
        lines.push(Line::from(vec![
            Span::styled(format!(" {}{}", l, " ".repeat(pad)), bg),
        ]));
    }
    while lines.len() < h as usize {
        lines.push(Line::styled(" ".repeat(w as usize), bg));
    }
    f.render_widget(Paragraph::new(lines), rect);
}

/// Draw the completion popup anchored to the cursor. Opens below the cursor
/// if there's room, else above.
fn render_completion_popup(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    use vix_lsp::lsp_types::CompletionItemKind;
    let Some(popup) = ed.completion_popup.as_ref() else { return };
    if popup.visible.is_empty() { return; }

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
    if h == 0 || width == 0 { return; }
    let rect = Rect::new(x, y, width, h);

    let bg = Style::default().bg(Color::Rgb(30, 30, 40)).fg(Color::White);
    let sel_bg = Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD);

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
fn scope_style(scope_idx: usize) -> Option<Style> {
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

fn render_content(f: &mut ratatui::Frame, area: Rect, ed: &mut Editor, hl_spans: &[HlSpan]) {
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
            } else { Vec::new() }
        } else { Vec::new() }
    } else { Vec::new() };

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for screen_row in 0..rows {
        let line_idx = ed.view_top + screen_row;
        if line_idx >= total_lines {
            lines.push(Line::from(Span::styled("~", Style::default().fg(Color::DarkGray))));
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
            if rel_s >= chars.len() { continue; }
            for slot in styles.iter_mut().take(rel_e).skip(rel_s) {
                *slot = Some(hl_style);
            }
        }

        // Apply visual selection highlight (layered over search highlight).
        if matches!(ed.mode, Mode::Visual | Mode::VisualLine) {
            let vrange = ed.visual_range();
            let sel_style = Style::default().bg(Color::Blue).fg(Color::White);
            if vrange.start < line_start_char + chars.len() + 1
                && vrange.end > line_start_char
            {
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
                    Mode::Insert => Style::default().add_modifier(Modifier::UNDERLINED).fg(Color::White),
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
                Mode::Insert => Style::default().add_modifier(Modifier::UNDERLINED).fg(Color::White),
                _ => Style::default().add_modifier(Modifier::REVERSED),
            };
            spans.push(Span::styled(" ", cursor_style));
        }

        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn render_statusline(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    let (line, col) = ed.buffer.char_to_line_col(ed.sel.head);
    let mode_style = match ed.mode {
        Mode::Normal => Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD),
        Mode::Insert => Style::default().bg(Color::Green).fg(Color::Black).add_modifier(Modifier::BOLD),
        Mode::Visual | Mode::VisualLine => Style::default().bg(Color::Magenta).fg(Color::Black).add_modifier(Modifier::BOLD),
        Mode::Command => Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD),
    };
    let path = ed.buffer.path().map(|p| p.display().to_string()).unwrap_or_else(|| "[No Name]".into());
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
        Span::styled(middle, Style::default().bg(Color::DarkGray).fg(Color::White)),
        Span::styled(right, Style::default().bg(Color::DarkGray).fg(Color::White)),
    ]);
    f.render_widget(Paragraph::new(line_widget), area);
}

fn render_picker(f: &mut ratatui::Frame, area: Rect, ed: &mut Editor) {
    let Some(p) = ed.picker.as_ref() else {
        ed.last_picker_rect = None;
        return;
    };

    // Centered overlay: ~80% wide, 2/3 tall. Clamp to minimums so it renders
    // on small terminals too.
    let w = ((area.width as u32 * 4 / 5).max(30)).min(area.width as u32) as u16;
    let h = ((area.height as u32 * 2 / 3).max(10)).min(area.height as u32) as u16;
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    let overlay = Rect::new(x, y, w, h);

    // Clear background then draw prompt + list.
    let bg = Style::default().bg(Color::Black).fg(Color::White);
    let blank: Vec<Line> = (0..h).map(|_| Line::raw(" ".repeat(w as usize))).collect();
    f.render_widget(Paragraph::new(blank).style(bg), overlay);

    let kind_label = match p.kind {
        PickerKind::Files => "files",
        PickerKind::Grep => "grep",
        PickerKind::Symbols => "symbols",
        PickerKind::Buffers => "buffers",
        PickerKind::CodeActions => "code actions",
        PickerKind::Jumps => "jumps",
    };
    let prompt = format!(" {} > {}", kind_label, p.query);
    let toggle_hint = match p.kind {
        PickerKind::Files => " <Tab>=grep ",
        PickerKind::Grep => " <Tab>=files ",
        _ => "",
    };
    let count = format!("{}{}/{} ", toggle_hint, p.matches.len(), p.items.len());
    let header_pad =
        (w as usize).saturating_sub(prompt.len() + count.len());
    let header = Line::from(vec![
        Span::styled(prompt, Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(" ".repeat(header_pad), Style::default().bg(Color::DarkGray)),
        Span::styled(count, Style::default().bg(Color::DarkGray).fg(Color::Gray)),
    ]);

    let list_rows = (h as usize).saturating_sub(1);
    // Scroll window so `selected` is visible.
    let scroll = if p.selected >= list_rows {
        p.selected + 1 - list_rows
    } else {
        0
    };

    let mut lines: Vec<Line> = Vec::with_capacity(h as usize);
    lines.push(header);
    for row in 0..list_rows {
        let idx = scroll + row;
        match p.matches.get(idx) {
            Some(&(item_idx, _)) => {
                let item = &p.items[item_idx];
                let is_sel = idx == p.selected;
                let style = if is_sel {
                    Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
                } else {
                    bg
                };
                let mut text = item.display.clone();
                if text.chars().count() > w as usize {
                    text = text.chars().take(w as usize).collect();
                }
                let pad = (w as usize).saturating_sub(text.chars().count());
                lines.push(Line::from(vec![
                    Span::styled(text, style),
                    Span::styled(" ".repeat(pad), style),
                ]));
            }
            None => lines.push(Line::raw("")),
        }
    }
    f.render_widget(Paragraph::new(lines), overlay);

    ed.last_picker_rect = Some(overlay);
    ed.last_picker_scroll = scroll;
}

fn render_cmdline(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
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
    execute!(term.backend_mut(), DisableMouseCapture, terminal::LeaveAlternateScreen)?;
    term.show_cursor()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use vix_lsp::lsp_types::{Position, Range, TextEdit};

    fn edit(sl: u32, sc: u32, el: u32, ec: u32, text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: Position { line: sl, character: sc },
                end: Position { line: el, character: ec },
            },
            new_text: text.into(),
        }
    }

    #[test]
    fn lsp_edits_are_undoable() {
        let mut ed = Editor::new(Buffer::from_text("hello world\n"));
        ed.apply_text_edits(&[edit(0, 6, 0, 11, "Rust")]);
        assert_eq!(ed.buffer.rope().to_string(), "hello Rust\n");
        ed.dispatch(Action::Undo);
        assert_eq!(ed.buffer.rope().to_string(), "hello world\n");
    }

    #[test]
    fn lsp_edits_do_not_pollute_dot_repeat() {
        // Set up a dot-repeatable action: `dw` at cursor 0 of "foo bar".
        let mut ed = Editor::new(Buffer::from_text("foo bar\n"));
        ed.dispatch(Action::Operate(PendingOp::Delete, Motion::WordForward, 1));
        assert_eq!(ed.buffer.rope().to_string(), "bar\n");
        let before = ed.last_change.clone();
        assert!(matches!(before, Some(RepeatAction::Operate { .. })));

        // Apply an "LSP-style" edit — should NOT overwrite last_change.
        ed.apply_text_edits(&[edit(0, 0, 0, 0, "baz ")]);
        assert_eq!(ed.buffer.rope().to_string(), "baz bar\n");

        // last_change must still point at the original `dw` action.
        match (&before, &ed.last_change) {
            (Some(RepeatAction::Operate { op: op_a, .. }), Some(RepeatAction::Operate { op: op_b, .. }))
                if op_a == op_b => {}
            _ => panic!("LSP edit polluted last_change: {:?}", ed.last_change),
        }

        // And `.` should still replay the `dw`: from current cursor (start of
        // "baz bar"), `dw` removes "baz ".
        ed.sel = Selection::at(0);
        ed.dispatch(Action::RepeatLastChange);
        assert_eq!(ed.buffer.rope().to_string(), "bar\n");
    }

    #[test]
    fn lsp_edit_then_undo_preserves_dot_repeat() {
        let mut ed = Editor::new(Buffer::from_text("foo bar\n"));
        ed.dispatch(Action::Operate(PendingOp::Delete, Motion::WordForward, 1));
        ed.apply_text_edits(&[edit(0, 0, 0, 0, "baz ")]);
        // Undo the LSP edit.
        ed.dispatch(Action::Undo);
        assert_eq!(ed.buffer.rope().to_string(), "bar\n");
        // Dot-repeat is still the original dw.
        assert!(matches!(ed.last_change, Some(RepeatAction::Operate { .. })));
    }
}
