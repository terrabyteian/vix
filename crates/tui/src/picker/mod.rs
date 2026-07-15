//! Picker overlay: types, pure state, and small helpers shared by the
//! `input`, `preview`, and `render` submodules.
use ratatui::style::Color;
use std::path::PathBuf;
use std::time::Instant;

use vix_picker::{grep, rescore_indices, scan_files, GrepItem, Utf32String};
use vix_syntax::HlSpan;

use crate::util::{count_chars, take_end, take_start, truncate_end};
use crate::Editor;

pub(crate) mod input;
pub(crate) mod preview;
pub(crate) mod render;

/// Overlay state for the file / grep pickers. The overlay owns input and
/// rendering while it's alive; dismissal returns control to Normal mode.
pub(crate) struct Picker {
    pub(crate) kind: PickerKind,
    pub(crate) mode: PickerMode,
    pub(crate) query: String,
    /// File-scan items. Only populated/consulted when `kind` is `Files`;
    /// doubles as the `<Tab>` Files↔Grep toggle cache so flipping back to
    /// Files doesn't rescan the tree.
    pub(crate) file_items: Vec<PickerItem>,
    /// Live grep-hit items. Only populated/consulted when `kind` is `Grep`.
    pub(crate) grep_items: Vec<PickerItem>,
    /// `(display, value, haystack)` tuples for every other picker kind
    /// (Symbols, Buffers, CodeActions, Jumps). Files/Grep use their own
    /// dedicated storage above instead — see `active_items`.
    pub(crate) items: Vec<PickerItem>,
    /// Scored subset of `active_items()` visible in the current list, plus
    /// the index back into it.
    pub(crate) matches: Vec<(usize, u32)>,
    pub(crate) selected: usize,
    /// Vertical scroll offset within the match list.
    pub(crate) scroll: usize,
    /// Set after a single `g` in Browse mode; the next `g` jumps to top.
    /// Cleared by any other key.
    pub(crate) pending_g: bool,
    /// Item indices marked by `<Space>` in Browse mode for batch opening.
    /// Item indices (not match indices) so marks survive query rescoring;
    /// cleared whenever the `items` vector is replaced (Tab toggle, grep
    /// refresh). Only Files/Grep pickers populate this.
    pub(crate) marked: std::collections::HashSet<usize>,
    /// Cached file preview for the currently-highlighted Files/Grep row.
    /// Holds the file's line-split source plus syntax spans so we don't
    /// re-read or re-parse on every render. Replaced when the highlighted
    /// row points at a different path.
    pub(crate) preview: Option<PreviewCache>,
    /// `selected` value the last time `refresh_preview` ran. Used to detect
    /// scroll movement so the preview rebuild can be debounced — see
    /// `preview_changed_at`.
    pub(crate) preview_last_seen_selected: Option<usize>,
    /// Wall-clock when `selected` last differed from `preview_last_seen_selected`.
    /// The preview only rebuilds after the selection has been stable for
    /// `PREVIEW_DEBOUNCE_MS`, so j/k/scroll spam doesn't trigger a parse
    /// per move. Initialized to picker-creation time so the first preview
    /// builds immediately.
    pub(crate) preview_changed_at: Instant,
    /// Set when the query changed and a rescore (Files) or regrep (Grep)
    /// is owed. Cleared once the refresh runs. The actual rescore is
    /// deferred until the query has been stable for
    /// `PICKER_REFRESH_DEBOUNCE_MS` so fast typing on large corpora
    /// doesn't trigger per-keystroke work.
    pub(crate) query_dirty_at: Option<Instant>,
}

/// Cached file preview data. Built lazily for the currently-selected
/// Files/Grep row and reused across renders until the selection moves to
/// a different path.
pub(crate) struct PreviewCache {
    pub(crate) path: PathBuf,
    /// File contents split by newline. Each entry omits the trailing `\n`.
    pub(crate) lines: Vec<String>,
    /// Byte offset where each line *starts* in the file. `len() == lines.len() + 1`,
    /// with the final entry equal to the file size — gives an exclusive end
    /// for the last line via simple subtraction.
    pub(crate) line_byte_starts: Vec<usize>,
    /// Tree-sitter highlight spans for the file (whole-file byte ranges).
    pub(crate) spans: Vec<HlSpan>,
    /// True if the file was too large or unreadable; `lines` then carries a
    /// short placeholder. We still cache so repeated renders don't retry I/O.
    pub(crate) placeholder: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PickerMode {
    Input,
    Browse,
}

#[derive(Clone, Debug)]
pub(crate) enum PickerKind {
    Files,
    Grep,
    Symbols,
    Buffers,
    CodeActions,
    Jumps,
}

/// Whether a picker kind renders as a fullscreen split (list + preview) or
/// a centered overlay.
pub(crate) enum PickerLayout {
    Full,
    Compact,
}

/// How a picker kind's `query` narrows `items` into `matches`. See
/// `Picker::rescore` for the actual per-kind logic; this just documents
/// which strategy a kind uses.
pub(crate) enum MatchMode {
    /// Smart-case substring match (Files).
    Substring,
    /// Nucleo fuzzy match (Symbols, Buffers, CodeActions, Jumps).
    Fuzzy,
    /// No re-ranking; items are already in the desired order (Grep, whose
    /// items are exact regex hits from an external walk).
    Identity,
}

/// Per-`PickerKind` configuration: display label, layout, matching
/// strategy, and which optional features (marks, preview, buffer actions)
/// the kind supports. Centralizes the per-kind branching that used to be
/// scattered across free functions and inline `match`es.
pub(crate) struct KindSpec {
    pub label: &'static str,
    pub layout: PickerLayout,
    /// Not yet consumed by `rescore` — it still switches on `PickerKind`
    /// directly (see its doc comment). Kept here so the per-kind matching
    /// strategy is documented in one place ahead of a later pass that
    /// dispatches on it.
    #[allow(dead_code)]
    pub match_mode: MatchMode,
    /// Whether `<Space>` multi-select / marks apply to this kind.
    pub supports_marks: bool,
    /// Whether the fullscreen split shows a preview pane for this kind.
    pub has_preview: bool,
    /// Whether `s`/`q`/`Q`/`r`/`R` buffer-management keys apply.
    pub buffer_actions: bool,
    /// Minimum query length before the picker's items are populated (Grep
    /// requires 2+ chars to avoid a whole-repo regex walk on an empty or
    /// 1-char pattern).
    pub min_query_len: usize,
}

impl PickerKind {
    pub(crate) fn spec(&self) -> &'static KindSpec {
        const FILES: KindSpec = KindSpec {
            label: "files",
            layout: PickerLayout::Full,
            match_mode: MatchMode::Substring,
            supports_marks: true,
            has_preview: true,
            buffer_actions: false,
            min_query_len: 0,
        };
        const GREP: KindSpec = KindSpec {
            label: "grep",
            layout: PickerLayout::Full,
            match_mode: MatchMode::Identity,
            supports_marks: true,
            has_preview: true,
            buffer_actions: false,
            min_query_len: 2,
        };
        const BUFFERS: KindSpec = KindSpec {
            label: "buffers",
            layout: PickerLayout::Full,
            match_mode: MatchMode::Fuzzy,
            supports_marks: false,
            has_preview: true,
            buffer_actions: true,
            min_query_len: 0,
        };
        const SYMBOLS: KindSpec = KindSpec {
            label: "symbols",
            layout: PickerLayout::Compact,
            match_mode: MatchMode::Fuzzy,
            supports_marks: false,
            has_preview: false,
            buffer_actions: false,
            min_query_len: 0,
        };
        const CODE_ACTIONS: KindSpec = KindSpec {
            label: "code actions",
            layout: PickerLayout::Compact,
            match_mode: MatchMode::Fuzzy,
            supports_marks: false,
            has_preview: false,
            buffer_actions: false,
            min_query_len: 0,
        };
        const JUMPS: KindSpec = KindSpec {
            label: "jumps",
            layout: PickerLayout::Compact,
            match_mode: MatchMode::Fuzzy,
            supports_marks: false,
            has_preview: false,
            buffer_actions: false,
            min_query_len: 0,
        };
        match self {
            PickerKind::Files => &FILES,
            PickerKind::Grep => &GREP,
            PickerKind::Buffers => &BUFFERS,
            PickerKind::Symbols => &SYMBOLS,
            PickerKind::CodeActions => &CODE_ACTIONS,
            PickerKind::Jumps => &JUMPS,
        }
    }
}

#[derive(Clone)]
pub(crate) struct PickerItem {
    pub(crate) display: String,
    pub(crate) value: PickerValue,
    pub(crate) haystack: Utf32String,
}

/// Selection payload. `File` is the selected path; `GrepHit` carries the
/// file + line number so we can jump after load.
#[derive(Clone, Debug)]
pub(crate) enum PickerValue {
    File(std::path::PathBuf),
    GrepHit {
        path: std::path::PathBuf,
        line: u64,
    },
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
pub(crate) fn scan_files_as_picker_items(cwd: &std::path::Path) -> Vec<PickerItem> {
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
///
/// The list shows `path:line` only — the matched line text is rendered in
/// the right-side preview pane anchored at the hit, so duplicating it in
/// the row would be visual noise.
pub(crate) fn grep_as_picker_items(cwd: &std::path::Path, query: &str) -> Vec<PickerItem> {
    let hits: Vec<GrepItem> = grep(cwd, query).unwrap_or_default();
    hits.into_iter()
        .map(|g| grep_hit_to_picker_item(cwd, g))
        .collect()
}

/// Build a `PickerItem` for a single grep hit. Used by both the sync path
/// (Tab toggle, initial open, Enter flush) and the async worker.
pub(crate) fn grep_hit_to_picker_item(cwd: &std::path::Path, g: GrepItem) -> PickerItem {
    let rel = g.path.strip_prefix(cwd).unwrap_or(&g.path);
    let display = format!("{}:{}", rel.display(), g.line);
    let haystack = Utf32String::from(display.as_str());
    PickerItem {
        display,
        value: PickerValue::GrepHit {
            path: g.path,
            line: g.line,
        },
        haystack,
    }
}

pub(crate) fn fit_path_display(path: &str, width: usize) -> String {
    if count_chars(path) <= width {
        return path.to_string();
    }
    if width <= 3 {
        return take_start(path, width);
    }

    let last_sep = path.rfind(['/', '\\']);
    let Some(last_sep) = last_sep else {
        return format!("...{}", take_end(path, width - 3));
    };
    let tail = &path[last_sep + 1..];
    let tail_len = count_chars(tail);
    if tail_len + 4 <= width {
        let first_sep = path.find(['/', '\\']).unwrap_or(last_sep);
        let first = &path[..first_sep];
        let candidate = format!("{first}/.../{tail}");
        if count_chars(&candidate) <= width {
            return candidate;
        }
        return format!(".../{tail}");
    }
    format!("...{}", take_end(path, width - 3))
}

/// Fit a `path:line` grep row into `width` columns. The line marker is
/// load-bearing, so we keep it intact and let the path fitter shave from
/// the front of the path as needed.
pub(crate) fn fit_grep_display(display: &str, line: u64, width: usize) -> String {
    if count_chars(display) <= width {
        return display.to_string();
    }
    let marker = format!(":{line}");
    let Some(marker_start) = display.rfind(&marker) else {
        return truncate_end(display, width);
    };
    let path = &display[..marker_start];
    let marker_width = count_chars(&marker);
    if width <= marker_width {
        return truncate_end(display, width);
    }
    let path_budget = width - marker_width;
    let path_text = fit_path_display(path, path_budget);
    format!("{path_text}{marker}")
}

pub(crate) fn fit_picker_row(item: &PickerItem, width: usize) -> String {
    match &item.value {
        PickerValue::File(_) => fit_path_display(&item.display, width),
        PickerValue::GrepHit { line, .. } => fit_grep_display(&item.display, *line, width),
        _ => truncate_end(&item.display, width),
    }
}

pub(crate) fn wrap_picker_detail(text: &str, width: usize, rows: usize) -> Vec<String> {
    if rows == 0 || width == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() && out.len() < rows {
        let end = (start + width).min(chars.len());
        out.push(chars[start..end].iter().collect::<String>());
        start = end;
    }
    if start < chars.len() {
        if let Some(last) = out.last_mut() {
            *last = if width <= 3 {
                take_start(last, width)
            } else {
                format!("{}...", take_start(last, width - 3))
            };
        }
    }
    out
}

impl Picker {
    /// Build a picker over `items` with every field at its default (Browse
    /// mode, empty query, no marks/preview/scroll), then rescore so
    /// `matches` reflects the (empty) query immediately. `items` is routed
    /// to the kind-appropriate storage: `file_items` for Files, `grep_items`
    /// for Grep, `items` for everything else — see `active_items`.
    pub(crate) fn new(kind: PickerKind, items: Vec<PickerItem>) -> Self {
        let mut p = Self {
            kind,
            mode: PickerMode::Browse,
            query: String::new(),
            file_items: Vec::new(),
            grep_items: Vec::new(),
            items: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            pending_g: false,
            marked: std::collections::HashSet::new(),
            preview: None,
            preview_last_seen_selected: None,
            preview_changed_at: Instant::now(),
            query_dirty_at: None,
        };
        match p.kind {
            PickerKind::Files => p.file_items = items,
            PickerKind::Grep => p.grep_items = items,
            _ => p.items = items,
        }
        p.rescore();
        p
    }

    /// Set the initial query and re-rescore. Builder-style for use at
    /// picker-construction time (e.g. the grep picker opened with a
    /// pre-filled pattern).
    pub(crate) fn with_query(mut self, q: &str) -> Self {
        self.query = q.to_string();
        self.rescore();
        self
    }

    /// The item list `matches` indexes into for the picker's current kind:
    /// `file_items` for Files, `grep_items` for Grep, `items` for everything
    /// else. Centralizes the per-kind storage split so renderers, mouse
    /// handling, and pick-commit don't need to know about it.
    pub(crate) fn active_items(&self) -> &[PickerItem] {
        match self.kind {
            PickerKind::Files => &self.file_items,
            PickerKind::Grep => &self.grep_items,
            _ => &self.items,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if delta < 0 {
            self.selected = self.selected.saturating_sub(delta.unsigned_abs());
        } else {
            self.selected =
                (self.selected + delta as usize).min(self.matches.len().saturating_sub(1));
        }
    }

    /// Re-score items against `self.query`. Caps visible matches at 1000 to
    /// keep the render loop snappy on large repos. Iterates the active
    /// item storage by reference and writes directly into `self.matches`,
    /// so a keystroke rescore on a 100k-file corpus doesn't allocate a
    /// clone per item.
    ///
    /// Grep is a special case: every item is already an exact regex hit, so
    /// running nucleo over the result list would only re-rank — at real cost
    /// for big result sets. We skip the fuzzy pass entirely and produce an
    /// identity match list (worker order, capped). Closer to ripgrep
    /// behavior, and zero per-result work on the UI thread.
    ///
    /// Files uses smart-case substring (not fuzzy) so a 100k-file corpus
    /// stays cheap per keystroke: no nucleo pattern parse, no per-item
    /// score, just a byte scan over the display string. Matches the user's
    /// "real grep on filenames" mental model.
    pub(crate) fn rescore(&mut self) {
        if matches!(self.kind, PickerKind::Grep) {
            self.matches.clear();
            for (i, _) in self.grep_items.iter().enumerate().take(1000) {
                self.matches.push((i, 0));
            }
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len().saturating_sub(1);
            }
            self.scroll = 0;
            return;
        }
        if matches!(self.kind, PickerKind::Files) {
            self.matches.clear();
            let q = &self.query;
            for (i, it) in self.file_items.iter().enumerate() {
                if substring_match_smart(&it.display, q).is_some() {
                    self.matches.push((i, 0));
                    if self.matches.len() >= 1000 {
                        break;
                    }
                }
            }
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len().saturating_sub(1);
            }
            self.scroll = 0;
            return;
        }
        rescore_indices(
            self.items
                .iter()
                .enumerate()
                .map(|(i, it)| (i, &it.haystack)),
            &self.query,
            1000,
            &mut self.matches,
        );
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
        self.scroll = 0;
    }
}

/// Smart-case substring search. Returns the byte offset of the first match
/// in `haystack`, or `None`. An empty `query` returns `Some(0)` (so callers
/// treat empty-query as "all match").
///
/// Smart case = case-sensitive when `query` contains any uppercase ASCII
/// letter, else ASCII case-insensitive. Non-ASCII queries fall through to
/// case-sensitive `str::find`; full Unicode case folding would mean
/// allocating per haystack on every keystroke, which defeats the purpose.
/// Editor targets (paths, identifiers) are predominantly ASCII so this is
/// the right trade.
pub(crate) fn substring_match_smart(haystack: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    if !query.is_ascii() || query.bytes().any(|b| b.is_ascii_uppercase()) {
        return haystack.find(query);
    }
    // Query is ASCII and all-lowercase. Case-fold the haystack byte-by-byte
    // as we scan. ASCII bytes can't appear inside UTF-8 continuation bytes,
    // so the byte offset we return is always on a char boundary even for
    // mixed-script haystacks.
    let h = haystack.as_bytes();
    let n = query.as_bytes();
    if h.len() < n.len() {
        return None;
    }
    let first = n[0];
    'outer: for i in 0..=h.len() - n.len() {
        if h[i].to_ascii_lowercase() != first {
            continue;
        }
        for j in 1..n.len() {
            if h[i + j].to_ascii_lowercase() != n[j] {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

pub(crate) const PICKER_ACCENT: Color = Color::Cyan;
pub(crate) const PICKER_ACCENT_HI: Color = Color::LightCyan;
pub(crate) const PICKER_BORDER: Color = Color::DarkGray;
pub(crate) const PICKER_DIM: Color = Color::Gray;

/// Cap the file size we'll attempt to read for previews. Keeps a single
/// keystroke from triggering a multi-MB read on a stray binary.
pub(crate) const PREVIEW_MAX_BYTES: usize = 256 * 1024;

/// Window after a selection change before we'll rebuild the preview. Keeps
/// fast j/k/scroll from triggering a per-move file read + parse on the
/// render thread; the run loop's 100 ms event poll guarantees we'll be
/// re-entered shortly after the user pauses.
pub(crate) const PREVIEW_DEBOUNCE_MS: u64 = 50;

/// Window after a query change before we run the deferred rescore (Files)
/// or regrep (Grep). Tuned so fast typing on large corpora doesn't pay the
/// per-keystroke cost: the user types, the prompt updates immediately, and
/// the match list catches up shortly after they pause.
pub(crate) const PICKER_REFRESH_DEBOUNCE_MS: u64 = 80;

impl Editor {
    pub fn picker_open(&self) -> bool {
        self.picker.is_some()
    }
    pub fn picker_query(&self) -> Option<&str> {
        self.picker.as_ref().map(|p| p.query.as_str())
    }
    pub fn picker_kind_label(&self) -> Option<&'static str> {
        self.picker.as_ref().map(|p| p.kind.spec().label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_path_fit_preserves_tail() {
        let row = fit_path_display("crates/tui/src/render_picker.rs", 24);
        assert!(count_chars(&row) <= 24, "row too wide: {row}");
        assert!(row.contains("..."), "expected ellipsis: {row}");
        assert!(
            row.ends_with("render_picker.rs"),
            "lost filename tail: {row}"
        );
    }

    #[test]
    fn picker_grep_fit_keeps_line_marker() {
        let row = fit_grep_display("crates/tui/src/render_picker.rs:128", 128, 24);
        assert!(count_chars(&row) <= 24, "row too wide: {row}");
        assert!(row.ends_with(":128"), "lost line marker: {row}");
        assert!(row.contains("..."), "expected truncation marker: {row}");
    }

    #[test]
    fn picker_detail_wrap_marks_truncated_text() {
        let rows = wrap_picker_detail("abcdefghijklmnopqrstuvwxyz", 10, 2);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| count_chars(row) <= 10));
        assert!(
            rows[1].ends_with("..."),
            "missing truncation marker: {rows:?}"
        );
    }
}
