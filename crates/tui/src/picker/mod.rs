//! Picker overlay: types, pure state, and small helpers shared by the
//! `input`, `preview`, and `render` submodules.
use std::path::{Path, PathBuf};
use std::time::Instant;

use vix_picker::{GrepItem, Scorer, Utf32String};
use vix_syntax::HlSpan;

use crate::util::{char_index_in_byte_range, count_chars, take_end, take_start, truncate_end};
use crate::Editor;

pub(crate) mod input;
pub(crate) mod preview;
pub(crate) mod render;

/// Overlay state for the omni / symbols / buffers / … pickers. The overlay
/// owns input and rendering while it's alive; dismissal returns control to
/// Normal mode.
///
/// The Omni kind blends two live sources into one ranked list: file names
/// (`file_items`, streamed by the scan worker) and file contents
/// (`grep_items`, streamed by the grep worker). A single `query` searches
/// both at once; `filter` narrows the blend to one source. Every other kind
/// (Symbols, Buffers, …) uses `items` and nucleo fuzzy matching.
pub(crate) struct Picker {
    pub(crate) kind: PickerKind,
    pub(crate) query: String,
    /// Which of the omni sources are visible: All (blend), Files (names
    /// only), or Content (grep hits only). Ignored by non-omni kinds.
    pub(crate) filter: SourceFilter,
    /// Omni file-name source. Filled incrementally by the streaming scan
    /// worker via `Editor::pump_picker_sources`. Referenced by
    /// `ItemRef::File`.
    pub(crate) file_items: Vec<PickerItem>,
    /// True once the streaming file scan has delivered its last batch (or
    /// no scan is in flight). Drives the "scanning…" indicator and the
    /// final ranked re-sort for a live query.
    pub(crate) file_items_complete: bool,
    /// Omni content source: live grep-hit items, filled incrementally by
    /// the streaming grep worker. Referenced by `ItemRef::Grep`.
    pub(crate) grep_items: Vec<PickerItem>,
    /// True once the streaming grep has delivered its last batch (or no
    /// grep is in flight).
    pub(crate) grep_items_complete: bool,
    /// The query `grep_items` was produced for. Lets the `<Tab>` filter
    /// cycle tell a warm grep cache from a stale one without re-running the
    /// walk.
    pub(crate) last_grep_query: Option<String>,
    /// Set when the content pattern failed to build (an invalid/incomplete
    /// regex mid-typing): the walk didn't run and the content list is
    /// empty. Cleared by the next query flush whose pattern parses.
    /// Rendered by the omnibox footer.
    pub(crate) grep_error: Option<String>,
    /// Recent-files source for the omni empty-query view. Loaded at open
    /// time from the per-project recent store, most-recent-first;
    /// referenced by `ItemRef::Recent`. Drives the empty-query, All-filter
    /// view (see `rescore_omni` / `showing_recents`): stored order is
    /// display order, so index 0 is both the most-recently-opened file and
    /// the top row.
    pub(crate) recent_items: Vec<PickerItem>,
    /// Rel-path display → recency rank (0 = most recent). Feeds a ranking
    /// bonus in `score_file_display` (non-empty query) and
    /// `score_file_recency` (empty query, Files filter) so recently-opened
    /// files float up in the file-name results.
    pub(crate) recent_rank: std::collections::HashMap<String, usize>,
    /// Count of `file_items` that matched the current query, maintained
    /// regardless of `filter` (so the Files count is honest even when the
    /// Content filter hides the rows). Not the same as the number of file
    /// rows in `matches`.
    pub(crate) file_match_count: usize,
    /// Count of `grep_items` (every grep hit is a match — the walk already
    /// filtered). Maintained regardless of `filter`.
    pub(crate) grep_match_count: usize,
    /// `(display, value, haystack)` tuples for every non-omni picker kind
    /// (Symbols, Buffers, CodeActions, Jumps). Referenced by
    /// `ItemRef::Item`.
    pub(crate) items: Vec<PickerItem>,
    /// Scored subset of the picker's sources visible in the current list,
    /// kept sorted at all times. Each entry is `(source ref, score)`: omni
    /// stores its baked-in i64 band score (see `omni_cmp` — the list order
    /// is descending score with deterministic tie keys, maintained
    /// continuously as batches stream in), while fuzzy kinds store the
    /// nucleo `u32` score widened to i64.
    pub(crate) matches: Vec<(ItemRef, i64)>,
    pub(crate) selected: usize,
    /// Vertical scroll offset within the match list.
    pub(crate) scroll: usize,
    /// Number of list rows the picker was last rendered with. Updated by
    /// `render_picker` on every render; used to size PageUp/PageDown jumps
    /// to whatever's actually on screen. Defaults to
    /// 0 before the first render — callers should treat 0 as "unknown" and
    /// fall back to a fixed page size.
    pub(crate) last_list_rows: usize,
    /// Source refs marked by `<C-Space>` for batch opening. Keyed by
    /// `ItemRef` (not match position) so marks survive query rescoring and
    /// filter cycling: a marked `File(3)` stays marked whichever filter is
    /// active. Grep marks are dropped when the grep source is replaced (a
    /// query-driven re-walk). Only the Omni picker populates this.
    pub(crate) marked: std::collections::HashSet<ItemRef>,
    /// Small MRU cache of built previews (front = most-recently used, cap
    /// `PREVIEW_LRU_CAP`). Keyed by `PreviewCache::key` — (buffer index,
    /// version) for Buffers, normalized file path for Omni rows. Selecting a
    /// row whose target is already cached promotes it to the front instantly;
    /// a miss builds and pushes to the front, evicting the tail. The renderer
    /// draws `previews.first()`.
    pub(crate) previews: Vec<PreviewCache>,
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
    /// Set when the query changed and a rescore (file names) or regrep
    /// (contents) is owed. Cleared once the refresh runs. The actual rescore is
    /// deferred until the query has been stable for
    /// `PICKER_REFRESH_DEBOUNCE_MS` so fast typing on large corpora
    /// doesn't trigger per-keystroke work.
    pub(crate) query_dirty_at: Option<Instant>,
    /// Persistent nucleo scorer for the Symbols/Buffers/CodeActions/Jumps
    /// fuzzy match (`rescore`'s fallback branch) and the fullscreen list's
    /// match-highlight pass. Held for the picker's lifetime so neither pays
    /// for a fresh `Matcher` (or, on a repeated query, a fresh `Pattern`
    /// parse) on every keystroke or render.
    pub(crate) scorer: Scorer,
}

/// Identity of a cached preview, used to look it up in the MRU. Buffers key
/// on the buffer index plus its version, so an edited buffer misses and
/// rebuilds. Omni rows key on the file's normalized path (see
/// [`omni_key_path`]), read from disk at build time.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum PreviewKey {
    Path(PathBuf),
    Buffer { idx: usize, version: u64 },
}

/// Cache-key path for an omni row: relative to the project root when
/// possible, so a File row (already root-relative) and a GrepHit row (an
/// absolute path from the walk) for the same file share one preview cache
/// entry.
pub(crate) fn omni_key_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// Cached file/buffer preview data. Built lazily for a selected row and kept
/// in the picker's preview MRU keyed by `key`.
pub(crate) struct PreviewCache {
    /// MRU lookup key.
    pub(crate) key: PreviewKey,
    /// Display path (pane header / file-name). For buffers this is the
    /// buffer's path or a synthetic `[No Name]`.
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

#[derive(Clone, Debug)]
pub(crate) enum PickerKind {
    /// Unified file-name + file-content finder. One query, two streamed
    /// sources, one ranked list; `<Tab>` cycles the source filter.
    Omni,
    Symbols,
    Buffers,
    CodeActions,
    Jumps,
}

/// A reference into one of the picker's parallel source vecs. `matches`
/// stores these so a single ranked list can blend rows from several sources
/// (the omni blend) or index a single source (every other kind). The
/// accessor `Picker::item` resolves one back to its `PickerItem`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ItemRef {
    /// Into `Picker::items` — Symbols/Buffers/CodeActions/Jumps.
    Item(usize),
    /// Into `Picker::file_items` — the omni file-name source.
    File(usize),
    /// Into `Picker::grep_items` — the omni content source.
    Grep(usize),
    /// Into `Picker::recent_items` — the omni empty-query, All-filter view
    /// (see `Picker::rescore_omni`).
    Recent(usize),
}

/// Which omni sources the current query draws from. `<Tab>` cycles
/// All → Files → Content → All.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum SourceFilter {
    /// Blend file names and file contents.
    #[default]
    All,
    /// File names only.
    Files,
    /// File contents (grep hits) only.
    Content,
}

/// Whether a picker kind renders as a fullscreen split (list + preview), the
/// fullscreen omnibox (full-width input box, result list with an optional
/// preview split, footer), or the legacy centered compact overlay.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerLayout {
    Full,
    Omni,
    Compact,
}

/// Per-`PickerKind` configuration: display label, layout, and which
/// optional features (marks, preview, buffer actions) the kind supports.
/// Centralizes the per-kind branching that used to be scattered across free
/// functions and inline `match`es.
pub(crate) struct KindSpec {
    pub label: &'static str,
    pub layout: PickerLayout,
    /// Whether `<Space>` multi-select / marks apply to this kind.
    pub supports_marks: bool,
    /// Whether the fullscreen split shows a preview pane for this kind.
    pub has_preview: bool,
    /// Whether `s`/`q`/`Q`/`r`/`R` buffer-management keys apply.
    pub buffer_actions: bool,
}

impl PickerKind {
    pub(crate) fn spec(&self) -> &'static KindSpec {
        // Omni renders through the dedicated omnibox layout; its preview
        // pane reads files from disk (see `picker::preview`).
        const OMNI: KindSpec = KindSpec {
            label: "omni",
            layout: PickerLayout::Omni,
            supports_marks: true,
            has_preview: true,
            buffer_actions: false,
        };
        const BUFFERS: KindSpec = KindSpec {
            label: "buffers",
            layout: PickerLayout::Full,
            supports_marks: false,
            has_preview: true,
            buffer_actions: true,
        };
        const SYMBOLS: KindSpec = KindSpec {
            label: "symbols",
            layout: PickerLayout::Compact,
            supports_marks: false,
            has_preview: false,
            buffer_actions: false,
        };
        const CODE_ACTIONS: KindSpec = KindSpec {
            label: "code actions",
            layout: PickerLayout::Compact,
            supports_marks: false,
            has_preview: false,
            buffer_actions: false,
        };
        const JUMPS: KindSpec = KindSpec {
            label: "jumps",
            layout: PickerLayout::Compact,
            supports_marks: false,
            has_preview: false,
            buffer_actions: false,
        };
        match self {
            PickerKind::Omni => &OMNI,
            PickerKind::Buffers => &BUFFERS,
            PickerKind::Symbols => &SYMBOLS,
            PickerKind::CodeActions => &CODE_ACTIONS,
            PickerKind::Jumps => &JUMPS,
        }
    }
}

/// Minimum query length before the omni content (grep) source runs. Below
/// this, a Content-filtered query shows a hint instead of walking the whole
/// repo, and the blended view is file-names-only. In regex mode the gate
/// measures the pattern *after* the `/` sigil (see [`parse_omni_query`]).
pub(crate) const MIN_CONTENT_QUERY_LEN: usize = 2;

/// A parsed omni query: search mode plus the pattern the content (grep)
/// source actually runs.
pub(crate) struct OmniQuery<'a> {
    /// True when the query began with the `/` sigil: regex, content-only.
    pub regex: bool,
    /// The query with any leading sigil stripped. Untrimmed — mirroring how
    /// the literal path passes the raw query through; trimming happens only
    /// at the `MIN_CONTENT_QUERY_LEN` gate.
    pub pattern: &'a str,
}

/// Split an omni query into mode + pattern. Regex iff the FIRST char is
/// `/`: the sigil is stripped and the remainder is a regex, content-only
/// search. A `/` anywhere else is an ordinary literal character. Compat
/// break: a leading `/` no longer literal-searches a slash — the escape
/// hatch is `/\/…`, a regex whose escaped slash matches a literal leading
/// slash.
pub(crate) fn parse_omni_query(q: &str) -> OmniQuery<'_> {
    match q.strip_prefix('/') {
        Some(rest) => OmniQuery {
            regex: true,
            pattern: rest,
        },
        None => OmniQuery {
            regex: false,
            pattern: q,
        },
    }
}

// --- Omni ranking bands -----------------------------------------------------
//
// One i64 score per candidate, higher = earlier. File-name hits live in the
// FILE_BAND (with a basename bonus, an earlier-offset preference, a shorter-
// path preference, and a recency bonus); content hits all score exactly
// GREP_BAND and are ordered by their `(path, line)` tie key (see
// `OmniTieKey`). The consequence, documented so the blend's feel is
// intentional: any realistic file hit (path under ~200 chars) outscores
// every content hit, so the omni list reads as "ranked name hits, then
// content hits in (path, line) order".

/// Base score for a file-name hit. Above `GREP_BAND` by a wide margin.
const FILE_BAND: i64 = 2_000_000;
/// Added when the first query token hits the path's *basename* (segment
/// after the last `/`) rather than only a parent directory.
const BASENAME_BONUS: i64 = 1_000_000;
/// Score for every content (grep) hit — a constant: within the band, the
/// `(path, line)` tie key alone orders the rows, so the rank is independent
/// of the grep worker's thread arrival order.
const GREP_BAND: i64 = 1_000_000;

/// Cap on the visible omni match list. Shared by the full rescore and the
/// streaming sorted inserts so both agree on which top-K survive: under the
/// `omni_cmp` total order, the settled top-K is the same whatever order the
/// candidates arrived in.
pub(crate) const OMNI_MATCH_CAP: usize = 1000;

/// Deterministic secondary key for omni score ties. Variant order is
/// load-bearing: at equal score, File sorts before Grep.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum OmniTieKey<'a> {
    /// Display path, lexicographic.
    File(&'a str),
    /// `(path, line)` ascending — the natural grep order.
    Grep(&'a std::path::Path, u64),
}

/// Resolve a match ref to its tie key. Only `File`/`Grep` refs ever reach a
/// scored sort: the recents view early-returns before sorting, and `Item` is
/// non-omni.
fn omni_tie_key<'a>(
    file_items: &'a [PickerItem],
    grep_items: &'a [PickerItem],
    r: ItemRef,
) -> OmniTieKey<'a> {
    match r {
        ItemRef::File(i) => OmniTieKey::File(&file_items[i].display),
        ItemRef::Grep(i) => match &grep_items[i].value {
            PickerValue::GrepHit { path, line } => OmniTieKey::Grep(path, *line),
            other => unreachable!("grep item without a GrepHit value: {other:?}"),
        },
        ItemRef::Item(_) | ItemRef::Recent(_) => {
            unreachable!("non-omni ref {r:?} in an omni scored sort")
        }
    }
}

/// The omni match list's total order: descending score, ties broken by
/// `OmniTieKey` ascending. Total (up to duplicate items), so the sorted list
/// — and its top-`OMNI_MATCH_CAP` prefix — is the same whatever order the
/// streaming workers delivered candidates in.
fn omni_cmp(
    file_items: &[PickerItem],
    grep_items: &[PickerItem],
    a: &(ItemRef, i64),
    b: &(ItemRef, i64),
) -> std::cmp::Ordering {
    b.1.cmp(&a.1).then_with(|| {
        omni_tie_key(file_items, grep_items, a.0).cmp(&omni_tie_key(file_items, grep_items, b.0))
    })
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

/// A streaming result channel from a background scan/grep worker, plus the
/// generation it was issued under. `Editor::pump_picker_sources` drains it
/// each tick, verifying the generation against the live cancel token before
/// applying anything (the belt to the worker-side suspenders).
pub(crate) struct PendingSource {
    pub(crate) rx: std::sync::mpsc::Receiver<Vec<PickerItem>>,
    pub(crate) gen: u64,
}

/// Wrap one scanned file as a picker item. Used by the streaming scan
/// worker (off the UI thread).
pub(crate) fn file_item_to_picker_item(fi: vix_picker::FileItem) -> PickerItem {
    PickerItem {
        display: fi.rel_path.to_string_lossy().into_owned(),
        value: PickerValue::File(fi.rel_path),
        haystack: fi.haystack,
    }
}

/// Longest grep snippet we retain per hit. A minified line can be hundreds of
/// KB; the row only ever shows a window of it, so cap the stored copy.
const MAX_GREP_SNIPPET_CHARS: usize = 500;

/// Build a `PickerItem` for a single grep hit. Used by the streaming grep
/// worker (off the UI thread). `display` is the hit's source snippet
/// (leading whitespace trimmed, length-capped); the `rel:line: snippet`
/// prefix is folded into `haystack` so the fuzzy search can still see the
/// path/line, and the renderer derives its own `rel:line` prefix from
/// `value`.
pub(crate) fn grep_hit_to_picker_item(cwd: &std::path::Path, g: GrepItem) -> PickerItem {
    let rel = g.path.strip_prefix(cwd).unwrap_or(&g.path);
    let mut snippet = g.text.trim_start().to_string();
    if snippet.chars().count() > MAX_GREP_SNIPPET_CHARS {
        snippet = snippet.chars().take(MAX_GREP_SNIPPET_CHARS).collect();
    }
    let haystack = Utf32String::from(format!("{}:{}: {}", rel.display(), g.line, snippet).as_str());
    PickerItem {
        display: snippet,
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

/// One omni content row split into its parts for rendering: a dimmed
/// `rel:line` prefix, the source snippet, and the char range within the
/// returned `snippet` that matched the query (for bold highlighting).
pub(crate) struct GrepRowLayout {
    pub prefix: String,
    pub snippet: String,
    /// Char range within `snippet` to highlight, or `None` when the query's
    /// first token didn't hit the snippet (e.g. it matched the path instead).
    pub hl: Option<std::ops::Range<usize>>,
}

/// Lay out one omni content row into `width` columns. The `rel:line` prefix
/// gets at most half the width (path-fitted from the front); the snippet
/// takes the rest. The query's first token is located in the snippet with
/// `substring_match_smart`; if the match would fall past the visible window
/// the snippet is shifted to an end-window that starts ~10 chars before the
/// match, so the hit is always on screen. `hl` is the match's char range
/// within the *returned* snippet.
pub(crate) fn layout_grep_row(
    rel: &str,
    line: u64,
    snippet: &str,
    query: &str,
    width: usize,
) -> GrepRowLayout {
    let prefix_budget = width / 2;
    let prefix = fit_path_display(&format!("{rel}:{line}"), prefix_budget);
    // One space separates the prefix from the snippet.
    let budget = width.saturating_sub(count_chars(&prefix) + 1);
    if budget == 0 {
        return GrepRowLayout {
            prefix,
            snippet: String::new(),
            hl: None,
        };
    }

    let chars: Vec<char> = snippet.chars().collect();
    let token = query.split_whitespace().next().unwrap_or("");
    // Match location (char index + char length) of the first token, if any.
    let matched = if token.is_empty() {
        None
    } else {
        substring_match_smart(snippet, token).map(|b| {
            let start = char_index_in_byte_range(snippet, b);
            let end = char_index_in_byte_range(snippet, b + token.len());
            (start, end - start)
        })
    };

    // Window start: 0 normally; shift left of a match that would sit off-screen.
    const LEAD: usize = 10;
    let start = match matched {
        Some((mc, mlen)) if mc + mlen > budget => mc.saturating_sub(LEAD),
        _ => 0,
    };
    let end = (start + budget).min(chars.len());
    let out: String = chars[start..end].iter().collect();

    // Re-base the highlight into `out`, clamped to what's visible.
    let hl = matched.and_then(|(mc, mlen)| {
        let hs = mc.checked_sub(start)?;
        let he = (mc + mlen).saturating_sub(start).min(count_chars(&out));
        if hs < he {
            Some(hs..he)
        } else {
            None
        }
    });

    GrepRowLayout {
        prefix,
        snippet: out,
        hl,
    }
}

pub(crate) fn fit_picker_row(item: &PickerItem, width: usize) -> String {
    match &item.value {
        PickerValue::File(_) => fit_path_display(&item.display, width),
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
    /// Build a picker over `items` with every field at its default (empty
    /// query, no marks/preview/scroll), then rescore so `matches` reflects
    /// the (empty) query immediately. `items` is routed to the
    /// kind-appropriate storage: `file_items` for Omni (its file-name
    /// source), `items` for everything else. The picker is always in typing
    /// mode: every printable key appends to `query`.
    pub(crate) fn new(kind: PickerKind, items: Vec<PickerItem>) -> Self {
        let mut p = Self {
            kind,
            query: String::new(),
            filter: SourceFilter::All,
            file_items: Vec::new(),
            file_items_complete: true,
            grep_items: Vec::new(),
            grep_items_complete: true,
            last_grep_query: None,
            grep_error: None,
            recent_items: Vec::new(),
            recent_rank: std::collections::HashMap::new(),
            file_match_count: 0,
            grep_match_count: 0,
            items: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            last_list_rows: 0,
            marked: std::collections::HashSet::new(),
            previews: Vec::new(),
            preview_last_seen_selected: None,
            preview_changed_at: Instant::now(),
            query_dirty_at: None,
            scorer: Scorer::new(),
        };
        match p.kind {
            PickerKind::Omni => p.file_items = items,
            _ => p.items = items,
        }
        p.rescore();
        p
    }

    /// Set the initial query and re-rescore. Builder-style for use at
    /// picker-construction time.
    pub(crate) fn with_query(mut self, q: &str) -> Self {
        self.query = q.to_string();
        self.rescore();
        self
    }

    /// Resolve one match ref back to its `PickerItem`, dispatching on which
    /// source vec it points into. Callers (enter/select, mouse hit-test,
    /// preview key, both render bodies, detail strip) use this instead of
    /// knowing about the per-source split.
    pub(crate) fn item(&self, r: ItemRef) -> &PickerItem {
        match r {
            ItemRef::Item(i) => &self.items[i],
            ItemRef::File(i) => &self.file_items[i],
            ItemRef::Grep(i) => &self.grep_items[i],
            ItemRef::Recent(i) => &self.recent_items[i],
        }
    }

    /// Total candidate count across the picker's sources — the `/total`
    /// denominator in the compact count line. Omni sums both streamed
    /// sources; every other kind has a single `items` list.
    pub(crate) fn candidate_count(&self) -> usize {
        match self.kind {
            PickerKind::Omni => self.file_items.len() + self.grep_items.len(),
            _ => self.items.len(),
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

    /// Blend the omni sources into one ranked match list for `self.query`,
    /// respecting `self.filter`, and refresh `file_match_count` /
    /// `grep_match_count`.
    ///
    /// Empty query is special-cased up front (see `showing_recents`):
    ///
    ///   - `All` with a non-empty `recent_items`: the recents view — matches
    ///     become `Recent(0..len)` in stored order (index 0 = most recent =
    ///     top row). No file/grep rows are shown; both counts still reflect
    ///     the full underlying sources so the footer stays honest.
    ///   - `All` with no recents (nothing opened yet this project): falls
    ///     through to the ordinary file-name scoring below, which — for an
    ///     empty query — ranks by scan order / shortest-path-first (see
    ///     `score_file_display`'s empty-token branch). Unchanged from
    ///     pre-recents behavior.
    ///   - `Content`: no matches (nothing to search below
    ///     `MIN_CONTENT_QUERY_LEN`; the footer shows a "type N+ chars" hint
    ///     instead).
    ///   - `Files`: every file, ranked by `score_file_recency` (recency
    ///     bonus, then shortest path) — recents bubble to the top even
    ///     though this filter doesn't use the dedicated recents view.
    ///
    /// For a non-empty query, file names use space-separated AND smart-case
    /// substring matching (`score_file_display`); every file item is scored
    /// so the counts stay honest, but a `File` ref is pushed only when the
    /// filter isn't `Content`. Content hits all score `GREP_BAND` and are
    /// pushed only when the filter isn't `Files`. Sorting by `omni_cmp`
    /// (descending score, deterministic tie keys — `(path, line)` for grep
    /// hits, display path for files), capped at `OMNI_MATCH_CAP`, produces
    /// the final order — realistic file hits always precede content hits
    /// (see the band constants), and identical inputs always produce the
    /// identical list. Selection / scroll are left untouched (callers
    /// manage them).
    fn rescore_omni(&mut self) {
        // Regex mode (leading `/` sigil): content-only. File rows drop out
        // entirely — count included — and the raw `/pattern` text is never
        // tokenized against file names. Every grep hit shows at GREP_BAND,
        // ordered by its `(path, line)` tie key. (`showing_recents` is
        // unaffected: a query starting with `/` is non-blank.)
        if self.regex_mode() {
            self.file_match_count = 0;
            self.grep_match_count = self.grep_items.len();
            let mut scored: Vec<(ItemRef, i64)> = (0..self.grep_items.len())
                .map(|i| (ItemRef::Grep(i), GREP_BAND))
                .collect();
            scored.sort_by(|a, b| omni_cmp(&self.file_items, &self.grep_items, a, b));
            scored.truncate(OMNI_MATCH_CAP);
            self.matches = scored;
            return;
        }

        let tokens: Vec<&str> = self.query.split_whitespace().collect();

        if tokens.is_empty() {
            match self.filter {
                SourceFilter::All if !self.recent_items.is_empty() => {
                    self.file_match_count = self.file_items.len();
                    self.grep_match_count = self.grep_items.len();
                    self.matches = (0..self.recent_items.len())
                        .map(|i| (ItemRef::Recent(i), 0))
                        .collect();
                    return;
                }
                SourceFilter::Content => {
                    self.file_match_count = self.file_items.len();
                    self.grep_match_count = self.grep_items.len();
                    self.matches = Vec::new();
                    return;
                }
                _ => {}
            }
        }

        let mut scored: Vec<(ItemRef, i64)> = Vec::new();

        // Empty query under the Files filter ranks by recency alone (see
        // `score_file_recency`) rather than the generic empty-token fallback
        // in `score_file_display`, so a recently-opened file floats to the
        // top even when the blended All view has no recents to show yet.
        let files_empty_query = tokens.is_empty() && self.filter == SourceFilter::Files;
        let mut file_count = 0usize;
        let want_files = self.filter != SourceFilter::Content;
        for (i, it) in self.file_items.iter().enumerate() {
            let score = if files_empty_query {
                Some(score_file_recency(&it.display, &self.recent_rank))
            } else {
                score_file_display(&it.display, &tokens, &self.recent_rank)
            };
            if let Some(score) = score {
                file_count += 1;
                if want_files {
                    scored.push((ItemRef::File(i), score));
                }
            }
        }
        self.file_match_count = file_count;

        self.grep_match_count = self.grep_items.len();
        if self.filter != SourceFilter::Files {
            for i in 0..self.grep_items.len() {
                scored.push((ItemRef::Grep(i), GREP_BAND));
            }
        }

        // The deterministic total order: descending score, ties broken by
        // `OmniTieKey` (display path for files, `(path, line)` for grep).
        scored.sort_by(|a, b| omni_cmp(&self.file_items, &self.grep_items, a, b));
        scored.truncate(OMNI_MATCH_CAP);
        self.matches = scored;
    }

    /// True when the omni query is in regex mode — its first char is the
    /// `/` sigil (see [`parse_omni_query`]).
    pub(crate) fn regex_mode(&self) -> bool {
        self.query.starts_with('/')
    }

    /// Whether the content (grep) source contributes rows: the `<Tab>`
    /// filter allows it, or regex mode forces it. Regex BYPASSES the filter
    /// without mutating it, so deleting the sigil restores normal blended
    /// search under whatever filter was active.
    pub(crate) fn content_visible(&self) -> bool {
        self.regex_mode() || self.filter != SourceFilter::Files
    }

    /// Char count of the content pattern — sigil stripped, trimmed. What
    /// the `MIN_CONTENT_QUERY_LEN` gate measures.
    pub(crate) fn content_pattern_len(&self) -> usize {
        parse_omni_query(&self.query).pattern.trim().chars().count()
    }

    /// True when the omni empty-query view is showing the recents list
    /// (`ItemRef::Recent` rows) rather than a file/grep view: query is
    /// empty, filter is `All`, and there's at least one recent file to
    /// show. Streaming batch-append (`append_file_matches`,
    /// `append_grep_matches`) and the footer both check this so they don't
    /// push rows under, or mislabel, the recents view.
    pub(crate) fn showing_recents(&self) -> bool {
        self.query.trim().is_empty()
            && self.filter == SourceFilter::All
            && !self.recent_items.is_empty()
    }

    /// Full rescore for a *changed query*: recompute matches and snap the
    /// selection back to the top (fzf behavior — a new query means the old
    /// selection is meaningless).
    pub(crate) fn rescore_query_changed(&mut self) {
        self.rescore();
        self.selected = 0;
        self.scroll = 0;
    }

    /// Insert one scored omni candidate at its `omni_cmp` position, keeping
    /// `matches` sorted and capped at `OMNI_MATCH_CAP`. A candidate whose
    /// position falls at or past the cap is dropped — deterministically,
    /// since the settled top-K under a total order is independent of arrival
    /// order.
    ///
    /// Selection: a highlight the user has *moved* (`selected > 0`) follows
    /// its item — an insert at or above it bumps `selected` by one so it
    /// stays on the same entry. The untouched top-row default (`selected ==
    /// 0`, where `rescore_query_changed` parks it) intentionally does not
    /// follow: it keeps tracking the best match so far, fzf-style, rather
    /// than pinning to whichever item happened to stream in first.
    fn insert_match_sorted(&mut self, r: ItemRef, score: i64) {
        let entry = (r, score);
        let pos = self.matches.partition_point(|m| {
            omni_cmp(&self.file_items, &self.grep_items, m, &entry) == std::cmp::Ordering::Less
        });
        if pos >= OMNI_MATCH_CAP {
            return;
        }
        self.matches.insert(pos, entry);
        if self.matches.len() > OMNI_MATCH_CAP {
            self.matches.pop();
        }
        if self.selected > 0 && pos <= self.selected {
            self.selected += 1;
        }
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    /// Incremental match update for a streaming file-scan batch: items from
    /// `from_idx` onward in `file_items` are new; score just those, bump
    /// `file_match_count`, and (unless the Content filter hides file rows)
    /// insert their hits at their ranked positions
    /// (`insert_match_sorted`), so the list stays sorted at all times
    /// during streaming. Uses the same scoring rule as `rescore_omni`
    /// (`score_file_recency` at empty query under the Files filter, else
    /// `score_file_display`) so a streamed insert lands exactly where the
    /// completion resort would put it. A user-moved highlight stays on its
    /// item — inserts above it shift the index, never the identity (see
    /// `insert_match_sorted` for the top-row default's behavior).
    pub(crate) fn append_file_matches(&mut self, from_idx: usize) {
        // Regex mode is content-only: no file rows and no count bump,
        // mirroring the `rescore_omni` regex branch (which zeroes the
        // count).
        if self.regex_mode() {
            return;
        }
        // Owned copy: `insert_match_sorted` below needs `&mut self` while
        // the tokens are alive.
        let query = self.query.clone();
        let tokens: Vec<&str> = query.split_whitespace().collect();
        // At empty query, All-filter with recents showing, new file rows
        // must not land under the recents view — only the honest count
        // updates. Both scorers match everything at empty query regardless
        // of filter, so the match check below is unaffected.
        let want_files = self.filter != SourceFilter::Content && !self.showing_recents();
        let files_empty_query = tokens.is_empty() && self.filter == SourceFilter::Files;
        for i in from_idx..self.file_items.len() {
            let score = if files_empty_query {
                Some(score_file_recency(
                    &self.file_items[i].display,
                    &self.recent_rank,
                ))
            } else {
                score_file_display(&self.file_items[i].display, &tokens, &self.recent_rank)
            };
            if let Some(score) = score {
                self.file_match_count += 1;
                if want_files {
                    self.insert_match_sorted(ItemRef::File(i), score);
                }
            }
        }
    }

    /// Incremental match update for a streaming grep batch: items from
    /// `from_idx` onward in `grep_items` are new. Refresh `grep_match_count`
    /// and (unless the Files filter hides content rows) insert them at
    /// their ranked `(path, line)` positions (`insert_match_sorted`), so
    /// the list stays sorted at all times during streaming and the final
    /// order is independent of the grep workers' arrival order.
    pub(crate) fn append_grep_matches(&mut self, from_idx: usize) {
        self.grep_match_count = self.grep_items.len();
        // The Files filter hides content rows outright (unless regex mode
        // bypasses it — see `content_visible`); the empty-query recents
        // view (All filter) also must not have grep rows pushed under it —
        // in practice the grep worker never runs at empty query (gated by
        // `MIN_CONTENT_QUERY_LEN`), so this is belt-and-suspenders.
        if !self.content_visible() || self.showing_recents() {
            return;
        }
        for i in from_idx..self.grep_items.len() {
            self.insert_match_sorted(ItemRef::Grep(i), GREP_BAND);
        }
    }

    /// Reconciliation pass after a source stream completes, keeping the
    /// highlight on the same *ref* wherever it lands. Streaming inserts
    /// already maintain the sorted order continuously, so this rebuilds the
    /// identical list by construction — it's kept as an invariant-restoring
    /// backstop (and it refreshes the honest match counts from scratch).
    pub(crate) fn rescore_omni_preserving_selection(&mut self) {
        let selected_ref = self.matches.get(self.selected).map(|&(r, _)| r);
        self.rescore_omni();
        if let Some(r) = selected_ref {
            self.selected = self.matches.iter().position(|&(x, _)| x == r).unwrap_or(0);
        }
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    /// Re-score the picker against `self.query`, capping visible matches at
    /// 1000 to keep the render loop snappy on large repos.
    ///
    /// Omni blends its streamed sources with a baked-in i64 ranking (see
    /// `rescore_omni`) — no nucleo pass, just a byte scan over file-name
    /// display strings plus the grep worker's discovery order. Every other
    /// kind runs nucleo fuzzy over `items`.
    pub(crate) fn rescore(&mut self) {
        if matches!(self.kind, PickerKind::Omni) {
            self.rescore_omni();
        } else {
            let mut out: Vec<(usize, u32)> = Vec::new();
            self.scorer.rescore_indices(
                self.items
                    .iter()
                    .enumerate()
                    .map(|(i, it)| (i, &it.haystack)),
                &self.query,
                1000,
                &mut out,
            );
            self.matches = out
                .into_iter()
                .map(|(i, s)| (ItemRef::Item(i), s as i64))
                .collect();
        }
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
        self.scroll = 0;
    }
}

/// Score one file-name row against the AND-token set for the omni FILE_BAND.
/// `None` = no match. Every space-separated token must `substring_match_smart`
/// the display path (empty token set matches everything, ranked shortest-
/// first in scan order). Ranking within the band, highest first:
///
///   `FILE_BAND + basename_bonus − min(first_token_offset·1000, 1_500_000)
///    − path_char_len + recency_bonus`
///
/// where `basename_bonus` is `BASENAME_BONUS` when the first token hits the
/// basename, and `recency_bonus` is `(50 − rank)·4_000` when the display path
/// is in `recent_rank` with `rank < 50` (0 = most recent). So `mod` ranks
/// `src/mod.rs` above `some/long/path/module_helpers.rs`, and a recently
/// opened file floats above an equally-good stranger.
///
/// Shared by the full rescore and the streaming batch append so both agree
/// on what matches.
fn score_file_display(
    disp: &str,
    tokens: &[&str],
    recent_rank: &std::collections::HashMap<String, usize>,
) -> Option<i64> {
    if tokens.is_empty() {
        // No query → keep scan order, shorter paths first. (Grep never
        // contributes at empty query, so these negative scores never collide
        // with the bands.)
        return Some(-(disp.chars().count() as i64));
    }
    // Every token must match somewhere in the path.
    if tokens
        .iter()
        .any(|t| substring_match_smart(disp, t).is_none())
    {
        return None;
    }
    let offset = substring_match_smart(disp, tokens[0]).unwrap_or(0);
    let base_start = disp.rfind('/').map(|p| p + 1).unwrap_or(0);
    let basename_hit = substring_match_smart(&disp[base_start..], tokens[0]).is_some();
    let len = disp.chars().count() as i64;
    let basename_bonus = if basename_hit { BASENAME_BONUS } else { 0 };
    let offset_penalty = ((offset as i64) * 1000).min(1_500_000);
    let recency_bonus = match recent_rank.get(disp) {
        Some(&rank) if rank < 50 => (50 - rank as i64) * 4_000,
        _ => 0,
    };
    Some(FILE_BAND + basename_bonus - offset_penalty - len + recency_bonus)
}

/// Score one file-name row for an empty query under the Files filter:
/// `recency_bonus − path_char_len`, using the same `(50 − rank)·4_000`
/// bonus shape as `score_file_display` for `rank < 50` (0 = most recent).
/// Recently-opened files float to the top; within a recency tier (or among
/// files with none) shorter paths sort first — simplest ordering that
/// satisfies both. Unlike `score_file_display`'s own empty-token branch
/// (used by the All-filter fallback when there's no recents source to show
/// instead), this always applies the recency bonus.
fn score_file_recency(disp: &str, recent_rank: &std::collections::HashMap<String, usize>) -> i64 {
    let len = disp.chars().count() as i64;
    let recency_bonus = match recent_rank.get(disp) {
        Some(&rank) if rank < 50 => (50 - rank as i64) * 4_000,
        _ => 0,
    };
    recency_bonus - len
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

/// Cap the file size we'll attempt to read for previews. Keeps a single
/// keystroke from triggering a multi-MB read on a stray binary.
pub(crate) const PREVIEW_MAX_BYTES: usize = 256 * 1024;

/// Window after a selection change before we'll rebuild the preview. Keeps
/// fast j/k/scroll from triggering a per-move file read + parse on the
/// render thread; the run loop's 100 ms event poll guarantees we'll be
/// re-entered shortly after the user pauses.
pub(crate) const PREVIEW_DEBOUNCE_MS: u64 = 50;

/// Max number of built previews kept in the picker's MRU cache. Small: the
/// user only ever revisits a handful of nearby rows, and each entry holds a
/// whole file's line-split source + syntax spans.
pub(crate) const PREVIEW_LRU_CAP: usize = 8;

/// Window after a query change before we run the deferred rescore (file
/// names) or regrep (contents). Tuned so fast typing on large corpora
/// doesn't pay the per-keystroke cost: the user types, the prompt updates
/// immediately, and the match list catches up shortly after they pause.
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

    /// The omni source filter as a stable string ("all"/"files"/"content"),
    /// or `None` when no picker (or a non-omni picker) is open. Test
    /// introspection.
    #[doc(hidden)]
    pub fn picker_filter_for_test(&self) -> Option<&'static str> {
        self.picker.as_ref().and_then(|p| {
            if !matches!(p.kind, PickerKind::Omni) {
                return None;
            }
            Some(match p.filter {
                SourceFilter::All => "all",
                SourceFilter::Files => "files",
                SourceFilter::Content => "content",
            })
        })
    }

    /// `(file_match_count, grep_match_count)` for the open picker, or
    /// `(0, 0)` when none is open. Test introspection.
    #[doc(hidden)]
    pub fn picker_counts_for_test(&self) -> (usize, usize) {
        self.picker
            .as_ref()
            .map(|p| (p.file_match_count, p.grep_match_count))
            .unwrap_or((0, 0))
    }

    /// The kind of the highlighted match's ref ("file"/"grep"/"item"/
    /// "recent"), or `None` when nothing is highlighted. Lets tests assert
    /// which omni source ranked first. Test introspection.
    #[doc(hidden)]
    pub fn picker_selected_source_for_test(&self) -> Option<&'static str> {
        self.picker.as_ref().and_then(|p| {
            p.matches.get(p.selected).map(|&(r, _)| match r {
                ItemRef::Item(_) => "item",
                ItemRef::File(_) => "file",
                ItemRef::Grep(_) => "grep",
                ItemRef::Recent(_) => "recent",
            })
        })
    }

    /// Display text of the match list's top row (index 0), or `None` when
    /// no picker is open or it has no matches. Test introspection — lets an
    /// end-to-end test assert which row rendered first (e.g. the
    /// empty-query recents view) without reaching into picker internals.
    #[doc(hidden)]
    pub fn picker_top_display_for_test(&self) -> Option<String> {
        self.picker
            .as_ref()
            .and_then(|p| p.matches.first().map(|&(r, _)| p.item(r).display.clone()))
    }

    /// The omni content source's error string (e.g. `"invalid regex"`), or
    /// `None` when there is no error or no picker. Test introspection.
    #[doc(hidden)]
    pub fn picker_grep_error_for_test(&self) -> Option<String> {
        self.picker.as_ref().and_then(|p| p.grep_error.clone())
    }

    /// Every match row resolved to an observable string, in list order:
    /// grep rows as `"{rel_path}:{line}:{snippet}"` (path relative to the
    /// project root the walk ran from, snippet as stored — leading
    /// whitespace trimmed), everything else as its display text. Resolved from the
    /// underlying items, not the rendered frame, so tests can assert the
    /// exact settled ordering of the blended list. Test introspection.
    #[doc(hidden)]
    pub fn picker_match_displays_for_test(&self) -> Vec<String> {
        let Some(p) = self.picker.as_ref() else {
            return Vec::new();
        };
        p.matches
            .iter()
            .map(|&(r, _)| match r {
                ItemRef::Grep(i) => {
                    let it = &p.grep_items[i];
                    match &it.value {
                        PickerValue::GrepHit { path, line } => {
                            let rel = path.strip_prefix(&self.root).unwrap_or(path);
                            format!("{}:{}:{}", rel.display(), line, it.display)
                        }
                        other => unreachable!("grep item without a GrepHit value: {other:?}"),
                    }
                }
                _ => p.item(r).display.clone(),
            })
            .collect()
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
    fn grep_row_prefix_capped_at_half_width() {
        let g = layout_grep_row("crates/tui/src/render.rs", 128, "let x = 1;", "let", 40);
        assert!(
            count_chars(&g.prefix) <= 20,
            "prefix over half width: {}",
            g.prefix
        );
        // Prefix keeps the line marker even when fitted.
        assert!(g.prefix.contains(":128"), "lost line marker: {}", g.prefix);
    }

    #[test]
    fn grep_row_highlights_the_match() {
        let g = layout_grep_row("a.rs", 1, "the needle is here", "needle", 40);
        let hl = g.hl.expect("expected a highlight range");
        let slice: String = g.snippet.chars().skip(hl.start).take(hl.len()).collect();
        assert_eq!(slice, "needle");
    }

    #[test]
    fn grep_row_end_windows_a_far_match() {
        // The match sits well past a narrow snippet budget; the window must
        // shift so the hit is visible, and hl must still point at it.
        let long = format!("{}TARGET tail", "x".repeat(80));
        let g = layout_grep_row("a.rs", 1, &long, "TARGET", 30);
        let hl = g.hl.expect("far match should still be highlighted");
        let slice: String = g.snippet.chars().skip(hl.start).take(hl.len()).collect();
        assert_eq!(slice, "TARGET");
        assert!(count_chars(&g.snippet) <= 30);
    }

    #[test]
    fn grep_row_no_match_has_no_highlight() {
        // Multi-token query whose first token hits the path, not the snippet.
        let g = layout_grep_row("a.rs", 1, "some code here", "zzz", 40);
        assert!(g.hl.is_none(), "no snippet match → no highlight");
    }

    #[test]
    fn grep_row_narrow_widths_do_not_panic() {
        let _ = layout_grep_row("really/long/path.rs", 999, "content here", "content", 10);
        let g = layout_grep_row("a.rs", 1, "content", "content", 0);
        assert!(g.snippet.is_empty());
        assert!(g.hl.is_none());
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

    /// Build an Omni picker whose file-name source is `paths`.
    fn files_picker(paths: &[&str]) -> Picker {
        let items = paths.iter().map(|d| file_item(d)).collect();
        Picker::new(PickerKind::Omni, items)
    }

    /// Display strings of the file rows, in rank order.
    fn ranked(p: &Picker) -> Vec<String> {
        p.matches
            .iter()
            .map(|&(r, _)| p.item(r).display.clone())
            .collect()
    }

    /// Resolve every match row to a stable identity: `path:line` for grep
    /// hits, the display path for everything else. Lets tests compare lists
    /// across pickers whose source-vec indices differ (different arrival
    /// orders).
    fn resolved(p: &Picker) -> Vec<String> {
        p.matches
            .iter()
            .map(|&(r, _)| match &p.item(r).value {
                PickerValue::GrepHit { path, line } => format!("{}:{}", path.display(), line),
                _ => p.item(r).display.clone(),
            })
            .collect()
    }

    /// Assert the match list is sorted under `omni_cmp` — the invariant the
    /// streaming inserts maintain continuously.
    fn assert_matches_sorted(p: &Picker) {
        for w in p.matches.windows(2) {
            assert_ne!(
                omni_cmp(&p.file_items, &p.grep_items, &w[0], &w[1]),
                std::cmp::Ordering::Greater,
                "matches out of order at {:?} vs {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn files_and_tokens_require_every_token() {
        let p = files_picker(&["src/mod.rs", "foo/bar.rs", "src/main.rs"]).with_query("src rs");
        let got = ranked(&p);
        assert_eq!(got.len(), 2, "only rows matching both tokens: {got:?}");
        assert!(got.contains(&"src/mod.rs".to_string()));
        assert!(got.contains(&"src/main.rs".to_string()));
        assert!(!got.contains(&"foo/bar.rs".to_string()));

        // Tokens are order-independent and must all be present.
        let p = files_picker(&["src/mod.rs", "src/main.rs"]).with_query("rs mod");
        assert_eq!(ranked(&p), vec!["src/mod.rs".to_string()]);
    }

    #[test]
    fn files_empty_query_matches_all_shortest_first() {
        let p = files_picker(&["aaa/longer.rs", "a.rs"]).with_query("");
        assert_eq!(
            ranked(&p),
            vec!["a.rs".to_string(), "aaa/longer.rs".to_string()]
        );
    }

    #[test]
    fn files_basename_hit_outranks_directory_hit() {
        // "foo" matches the directory of the first and the basename of the
        // second; the basename hit wins despite its later offset.
        let p = files_picker(&["foo/zzz.rs", "aaa/foo.rs"]).with_query("foo");
        assert_eq!(
            ranked(&p),
            vec!["aaa/foo.rs".to_string(), "foo/zzz.rs".to_string()]
        );
    }

    #[test]
    fn files_shorter_basename_hit_ranks_first() {
        // The canonical example: `mod` ranks the short basename hit above the
        // long one.
        let p = files_picker(&["some/long/path/module_helpers.rs", "src/mod.rs"]).with_query("mod");
        assert_eq!(ranked(&p)[0], "src/mod.rs".to_string());
    }

    #[test]
    fn recency_bonus_floats_a_recent_file_up() {
        // Two equally-plausible basename hits for "main"; the one marked most
        // recent (rank 0) must sort first thanks to the recency bonus.
        let mut p = files_picker(&["a/main.rs", "b/main.rs"]);
        p.recent_rank.insert("b/main.rs".to_string(), 0);
        let p = p.with_query("main");
        assert_eq!(ranked(&p)[0], "b/main.rs".to_string());
    }

    /// Build an Omni picker with a file-name source plus a recents source.
    /// `recents` is inserted in the given order as `recent_items` /
    /// `recent_rank` (index 0 = most recent), mirroring
    /// `Editor::load_recents_into`'s most-recent-first convention.
    fn picker_with_recents(files: &[&str], recents: &[&str]) -> Picker {
        let mut p = files_picker(files);
        for (rank, d) in recents.iter().enumerate() {
            p.recent_rank.insert(d.to_string(), rank);
            p.recent_items.push(file_item(d));
        }
        // `Picker::new` already rescored before `recent_items` existed
        // (mirroring the real `load_recents_into`-before-`with_query`
        // ordering) — redo it now that recents are in place.
        p.rescore();
        p
    }

    #[test]
    fn empty_query_all_filter_shows_recents_in_stored_order() {
        // Recents present at empty query, All filter: the view is entirely
        // `Recent` refs in stored order (index 0 = most recent = top row),
        // not the file list, even though file_items has its own entries.
        let p = picker_with_recents(&["a.rs", "b.rs"], &["newest.rs", "older.rs"]);
        assert_eq!(p.matches.len(), 2);
        assert!(p
            .matches
            .iter()
            .all(|&(r, _)| matches!(r, ItemRef::Recent(_))));
        assert_eq!(p.item(p.matches[0].0).display, "newest.rs");
        assert_eq!(p.item(p.matches[1].0).display, "older.rs");
    }

    #[test]
    fn empty_query_no_recents_falls_back_to_file_list() {
        // No recents recorded: the empty-query view is the ordinary file
        // fallback (shortest-path-first), all `File` refs.
        let p = files_picker(&["aaa/longer.rs", "a.rs"]);
        assert!(p.recent_items.is_empty());
        assert!(p
            .matches
            .iter()
            .all(|&(r, _)| matches!(r, ItemRef::File(_))));
        assert_eq!(
            ranked(&p),
            vec!["a.rs".to_string(), "aaa/longer.rs".to_string()]
        );
    }

    #[test]
    fn empty_query_files_filter_ranks_by_recency_then_length() {
        // A recent file with a LONGER path must still outrank a shorter
        // non-recent one: recency dominates, length only breaks ties.
        let mut p = picker_with_recents(
            &["src/main.rs", "some/long/deeply/nested/recent_module.rs"],
            &["some/long/deeply/nested/recent_module.rs"],
        );
        p.filter = SourceFilter::Files;
        p.rescore();
        assert!(p
            .matches
            .iter()
            .all(|&(r, _)| matches!(r, ItemRef::File(_))));
        assert_eq!(
            ranked(&p)[0],
            "some/long/deeply/nested/recent_module.rs".to_string(),
            "recency must outrank a shorter, non-recent path"
        );
    }

    #[test]
    fn empty_query_content_filter_has_no_matches() {
        let mut p = files_picker(&["a.rs"]);
        p.filter = SourceFilter::Content;
        p.rescore();
        assert!(p.matches.is_empty());
    }

    #[test]
    fn streaming_file_append_at_empty_query_does_not_displace_recents() {
        let mut p = picker_with_recents(&["a.rs"], &["recent.rs"]);
        assert_eq!(p.matches.len(), 1);
        assert!(matches!(p.matches[0].0, ItemRef::Recent(_)));

        // A file-scan batch streams in while the recents view is showing.
        let from = p.file_items.len();
        p.file_items.push(file_item("brand_new.rs"));
        p.append_file_matches(from);

        // The recents view is untouched; only the honest count moved.
        assert_eq!(p.matches.len(), 1, "recents view must not gain file rows");
        assert!(matches!(p.matches[0].0, ItemRef::Recent(_)));
        assert_eq!(p.file_match_count, 2, "count still tracks every file");
    }

    #[test]
    fn source_completion_rescore_keeps_recents_view() {
        let mut p = picker_with_recents(&["a.rs"], &["recent.rs"]);
        p.file_items.push(file_item("brand_new.rs"));
        p.rescore_omni_preserving_selection();
        assert_eq!(p.matches.len(), 1, "still just the recents row");
        assert!(matches!(p.matches[0].0, ItemRef::Recent(_)));
        assert_eq!(p.item(p.matches[0].0).display, "recent.rs");
    }

    fn file_item(d: &str) -> PickerItem {
        PickerItem {
            display: d.to_string(),
            value: PickerValue::File(d.into()),
            haystack: Utf32String::from(d),
        }
    }

    fn grep_item(display: &str, line: u64) -> PickerItem {
        PickerItem {
            display: display.to_string(),
            value: PickerValue::GrepHit {
                path: display.into(),
                line,
            },
            haystack: Utf32String::from(display),
        }
    }

    #[test]
    fn append_file_matches_keeps_selection_on_its_item() {
        let mut p = files_picker(&["aaa.rs", "abb.rs", "abc.rs"]).with_query("a");
        assert_eq!(p.matches.len(), 3);
        p.selected = 2;
        let followed = p.item(p.matches[p.selected].0).display.clone();
        // A streamed batch arrives: two more matches, one non-match.
        let from = p.file_items.len();
        p.file_items.push(file_item("axe.rs"));
        p.file_items.push(file_item("zzz.txt"));
        p.file_items.push(file_item("arm.rs"));
        p.append_file_matches(from);
        assert_eq!(p.matches.len(), 5, "two of three new items match");
        assert_eq!(p.file_match_count, 5);
        // Sorted inserts may shift the highlighted *row number* (an insert
        // above it bumps the index by design) — the highlighted *item* must
        // never change.
        assert_eq!(
            p.item(p.matches[p.selected].0).display,
            followed,
            "streaming insert must keep the highlight on its item"
        );
        assert_matches_sorted(&p);
    }

    #[test]
    fn append_file_matches_respects_the_cap() {
        let mut p = files_picker(&[]).with_query("");
        for i in 0..1100 {
            p.file_items.push(file_item(&format!("f{i}.rs")));
        }
        p.append_file_matches(0);
        assert_eq!(p.matches.len(), 1000);
        assert_eq!(p.file_match_count, 1100, "counts ignore the visible cap");
        assert_matches_sorted(&p);
        // The kept rows are the comparator's top-K, not an arrival-order
        // prefix: a from-scratch rescore produces the identical list.
        let streamed = ranked(&p);
        p.rescore();
        assert_eq!(ranked(&p), streamed, "streamed cap == rescored top-K");
    }

    #[test]
    fn omni_blends_names_first_then_contents() {
        // A file NAMED hello.txt and a content hit in another file: the
        // file-name row ranks first, both counts are nonzero.
        let mut p = files_picker(&["hello.txt", "other.rs"]);
        p.grep_items.push(grep_item("world.rs:3", 3));
        let p = p.with_query("hello");
        assert_eq!(p.file_match_count, 1);
        assert_eq!(p.grep_match_count, 1);
        // File hit outranks content hit (band ordering).
        assert!(matches!(p.matches[0].0, ItemRef::File(_)));
        assert!(matches!(p.matches[1].0, ItemRef::Grep(_)));
    }

    #[test]
    fn filter_hides_the_other_source() {
        let mut p = files_picker(&["hello.txt"]);
        p.grep_items.push(grep_item("world.rs:3", 3));
        p.filter = SourceFilter::Files;
        p.rescore();
        assert!(p
            .matches
            .iter()
            .all(|&(r, _)| matches!(r, ItemRef::File(_))));
        assert_eq!(p.grep_match_count, 1, "count stays honest under the filter");

        p.filter = SourceFilter::Content;
        p.rescore();
        assert!(p
            .matches
            .iter()
            .all(|&(r, _)| matches!(r, ItemRef::Grep(_))));
        assert_eq!(p.file_match_count, 1, "count stays honest under the filter");
    }

    #[test]
    fn completion_resort_keeps_highlight_on_the_same_item() {
        // Scrambled streamed arrival: the weak match lands first, the
        // basename hit later. Sorted insert ranks each hit immediately, so
        // the strong hit takes row 0 the moment it arrives; the completion
        // resort then rebuilds the identical list and must keep the
        // highlight on whichever item it was parked on.
        let mut p = files_picker(&[]).with_query("mod");
        let from = p.file_items.len();
        p.file_items
            .push(file_item("some/long/path/module_helpers.rs"));
        p.file_items.push(file_item("src/mod.rs"));
        p.append_file_matches(from);
        assert_eq!(p.matches.len(), 2);
        // Already ranked mid-stream: the later arrival sorted to the top.
        assert_eq!(p.item(p.matches[0].0).display, "src/mod.rs");
        p.selected = 1; // park the highlight on the weak match
        let followed = p.item(p.matches[p.selected].0).display.clone();
        p.rescore_omni_preserving_selection();
        assert_eq!(p.item(p.matches[0].0).display, "src/mod.rs");
        assert_eq!(p.item(p.matches[p.selected].0).display, followed);
    }

    #[test]
    fn grep_rows_rank_by_path_then_line() {
        // Scrambled arrival order; the (path, line) tie key alone decides
        // the rank.
        let mut p = files_picker(&[]).with_query("xx");
        let from = p.grep_items.len();
        p.grep_items.push(grep_item("b.rs", 2));
        p.grep_items.push(grep_item("a.rs", 9));
        p.grep_items.push(grep_item("a.rs", 1));
        p.append_grep_matches(from);
        assert_eq!(resolved(&p), vec!["a.rs:1", "a.rs:9", "b.rs:2"]);
    }

    #[test]
    fn streaming_batches_insert_sorted_never_shuffle() {
        let mut p = files_picker(&["src/mod.rs"]).with_query("mod");
        // Mixed file/grep batches in scrambled arrival order. After every
        // batch the list is fully sorted and each pre-existing row keeps its
        // relative order — the list only ever gains rows, never shuffles.
        let batches: [&[(&str, Option<u64>)]; 3] = [
            &[("z/mod.rs", None), ("b.rs", Some(4))],
            &[("a/mod.rs", None), ("a.rs", Some(7))],
            &[("a.rs", Some(2)), ("m/mod.rs", None)],
        ];
        for batch in batches {
            let before = resolved(&p);
            let file_from = p.file_items.len();
            let grep_from = p.grep_items.len();
            for &(path, line) in batch {
                match line {
                    Some(line) => p.grep_items.push(grep_item(path, line)),
                    None => p.file_items.push(file_item(path)),
                }
            }
            p.append_file_matches(file_from);
            p.append_grep_matches(grep_from);
            assert_matches_sorted(&p);
            let after = resolved(&p);
            let survivors: Vec<String> = after
                .iter()
                .filter(|id| before.contains(id))
                .cloned()
                .collect();
            assert_eq!(survivors, before, "existing rows must keep relative order");
        }
    }

    #[test]
    fn arrival_order_does_not_change_final_matches() {
        // Same corpus, different batch permutations and groupings — the
        // lists must agree both mid-stream (sorted inserts) and after the
        // completion resort.
        let files = ["src/mod.rs", "a/mod.rs", "z/mod.rs"];
        let greps: [(&str, u64); 4] = [("b.rs", 2), ("a.rs", 9), ("a.rs", 1), ("c.rs", 5)];
        let build = |file_order: &[usize], grep_batches: &[&[usize]]| {
            let mut p = files_picker(&[]).with_query("mod");
            let from = p.file_items.len();
            for &i in file_order {
                p.file_items.push(file_item(files[i]));
            }
            p.append_file_matches(from);
            for &batch in grep_batches {
                let from = p.grep_items.len();
                for &i in batch {
                    let (path, line) = greps[i];
                    p.grep_items.push(grep_item(path, line));
                }
                p.append_grep_matches(from);
            }
            p
        };
        let mut a = build(&[0, 1, 2], &[&[0, 1], &[2, 3]]);
        let mut b = build(&[2, 0, 1], &[&[3], &[1, 0, 2]]);
        assert_eq!(resolved(&a), resolved(&b), "mid-stream lists must agree");
        a.rescore_omni_preserving_selection();
        b.rescore_omni_preserving_selection();
        assert_eq!(resolved(&a), resolved(&b), "post-resort lists must agree");
    }

    #[test]
    fn cap_truncation_is_deterministic() {
        // 1100 grep hits — 100 over the cap — arriving in two different
        // scrambled orders (stride walks; 7 and 13 are coprime with 1100,
        // so each visits every index once). Both pickers must keep the
        // (path, line)-first 1000.
        let hits: Vec<String> = (0..1100).map(|i| format!("p{i:04}.rs")).collect();
        let build = |order: &dyn Fn(usize) -> usize| {
            let mut p = files_picker(&[]).with_query("xx");
            let from = p.grep_items.len();
            for i in 0..hits.len() {
                p.grep_items.push(grep_item(&hits[order(i)], 1));
            }
            p.append_grep_matches(from);
            p
        };
        let a = build(&|i| (i * 7) % 1100);
        let b = build(&|i| (i * 13) % 1100);
        assert_eq!(a.matches.len(), 1000);
        let expect: Vec<String> = (0..1000).map(|i| format!("p{i:04}.rs:1")).collect();
        assert_eq!(
            resolved(&a),
            expect,
            "kept rows are the (path,line)-first 1000"
        );
        assert_eq!(resolved(&a), resolved(&b));
    }

    #[test]
    fn file_score_ties_break_lexicographically() {
        // Identical band scores (same length, offset, and basename hit):
        // the display-path tie key decides, so the order is alphabetical —
        // and, crucially, independent of scan order.
        let p = files_picker(&["b/mod.rs", "a/mod.rs"]).with_query("mod");
        assert_eq!(
            ranked(&p),
            vec!["a/mod.rs".to_string(), "b/mod.rs".to_string()]
        );
    }

    #[test]
    fn sorted_insert_keeps_selection_on_item() {
        // The user parks the highlight below the top row; a batch that sorts
        // entirely above it streams in.
        let mut p = files_picker(&["mm/mod.rs", "zz/mod.rs"]).with_query("mod");
        p.selected = 1;
        let followed = p.item(p.matches[p.selected].0).display.clone();
        let from = p.file_items.len();
        p.file_items.push(file_item("aa/mod.rs"));
        p.file_items.push(file_item("bb/mod.rs"));
        p.append_file_matches(from);
        assert_eq!(p.matches.len(), 4);
        assert_eq!(p.selected, 3, "index shifted by the two inserts above");
        assert_eq!(
            p.item(p.matches[p.selected].0).display,
            followed,
            "the highlight follows its item, not its row number"
        );
        assert_matches_sorted(&p);
    }

    #[test]
    fn sorted_insert_leaves_the_default_top_row_selection_alone() {
        // An untouched selection (row 0, where every query change parks it)
        // must keep tracking the best match so far as rows stream in above,
        // fzf-style — not pin to whichever item happened to arrive first.
        let mut p = files_picker(&["zz/mod.rs"]).with_query("mod");
        assert_eq!(p.selected, 0);
        let from = p.file_items.len();
        p.file_items.push(file_item("aa/mod.rs"));
        p.file_items.push(file_item("bb/mod.rs"));
        p.append_file_matches(from);
        assert_eq!(p.selected, 0, "default selection stays on the top row");
        assert_eq!(p.item(p.matches[0].0).display, "aa/mod.rs");
    }

    #[test]
    fn parse_omni_query_strips_leading_slash_only() {
        let q = parse_omni_query("/foo");
        assert!(q.regex);
        assert_eq!(q.pattern, "foo");

        let q = parse_omni_query("foo");
        assert!(!q.regex);
        assert_eq!(q.pattern, "foo");

        // A slash anywhere but first is an ordinary character.
        let q = parse_omni_query("a/b");
        assert!(!q.regex);
        assert_eq!(q.pattern, "a/b");

        // Bare sigil: regex mode with an empty pattern (below the gate).
        let q = parse_omni_query("/");
        assert!(q.regex);
        assert_eq!(q.pattern, "");

        // Only the FIRST char counts — a leading space stays literal, and
        // the pattern is untrimmed.
        let q = parse_omni_query(" /foo");
        assert!(!q.regex);
        assert_eq!(q.pattern, " /foo");
    }

    #[test]
    fn regex_query_drops_file_rows() {
        // File names that would match "hello" plus grep hits in scrambled
        // arrival order: under a `/hello` query only Grep refs survive,
        // ordered by their (path, line) tie key, and the file count zeroes.
        let mut p = files_picker(&["hello.txt", "other.rs"]);
        p.grep_items.push(grep_item("b.rs", 2));
        p.grep_items.push(grep_item("a.rs", 9));
        p.grep_items.push(grep_item("a.rs", 1));
        let p = p.with_query("/hello");
        assert_eq!(p.file_match_count, 0);
        assert!(p
            .matches
            .iter()
            .all(|&(r, _)| matches!(r, ItemRef::Grep(_))));
        assert_eq!(resolved(&p), vec!["a.rs:1", "a.rs:9", "b.rs:2"]);
    }

    #[test]
    fn regex_append_file_matches_is_a_noop() {
        let mut p = files_picker(&[]).with_query("/hello");
        let from = p.file_items.len();
        p.file_items.push(file_item("hello.txt"));
        p.append_file_matches(from);
        assert!(p.matches.is_empty(), "no file rows in regex mode");
        assert_eq!(p.file_match_count, 0, "no count bump either");
    }
}
