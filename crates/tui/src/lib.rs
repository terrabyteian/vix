use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
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
    compile_search, find_all_in_lines, Buffer, Case, FindDirection, FindKind, History, InsertPos,
    JumpList, Mode, Motion, NormalKeyState, RepeatAction, SearchDirection, Selection, TextObject,
    TextObjectKind, Transaction,
};

mod buffers;
mod completion;
mod dispatch;
mod ex;
pub mod help;
mod input;
mod jumps;
mod lsp;
mod picker;
mod search;
pub mod testing;
mod util;
use vix_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};
use vix_lsp::{LspClient, RequestId};
use vix_syntax::{HlSpan, Language, SyntaxState, HIGHLIGHT_NAMES};

use buffers::BufferSave;
use completion::CompletionPopup;
use lsp::{diag_summary, sev_rank, LspDocState, PendingRequest};
use picker::render::{render_picker, render_picker_fullscreen};
use picker::{is_fullscreen_picker_kind, Picker, PickerItem};

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
