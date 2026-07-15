//! Picker preview machinery: reading/caching file or buffer contents and
//! syntax-highlighting them for the fullscreen picker's preview pane.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vix_core::Buffer;
use vix_syntax::{HlSpan, Language, SyntaxState};

use crate::picker::{PickerKind, PickerLayout, PickerValue, PreviewCache, PreviewKey};
use crate::picker::{PREVIEW_DEBOUNCE_MS, PREVIEW_LRU_CAP, PREVIEW_MAX_BYTES};
use crate::Editor;

impl Editor {
    /// Get-or-create a `SyntaxState` for `lang` from the per-language cache.
    /// Returns `None` if construction fails (bad query, etc.). Reused across
    /// preview rebuilds so the expensive query compile happens once per
    /// language per editor session.
    pub(crate) fn cached_syntax(&mut self, lang: Language) -> Option<&mut SyntaxState> {
        if let std::collections::hash_map::Entry::Vacant(e) = self.preview_syntax.entry(lang) {
            if let Ok(s) = SyntaxState::new(lang) {
                e.insert(s);
            }
        }
        self.preview_syntax.get_mut(&lang)
    }
}

impl PreviewCache {
    fn placeholder(path: &Path, msg: &str) -> Self {
        Self {
            key: PreviewKey::Path(path.to_path_buf()),
            path: path.to_path_buf(),
            lines: vec![msg.to_string()],
            line_byte_starts: vec![0, msg.len()],
            spans: Vec::new(),
            placeholder: true,
        }
    }
}

/// Read `path` and return its source text, or a fully-formed placeholder
/// `PreviewCache` if it's unreadable / too large / binary / non-UTF8.
///
/// Reads at most `PREVIEW_MAX_BYTES + 1` so we don't slurp a multi-MB file
/// just to throw it away at the size check below.
pub(crate) fn read_preview_source(path: &Path) -> Result<String, Box<PreviewCache>> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| {
        Box::new(PreviewCache::placeholder(
            path,
            &format!("(cannot read: {e})"),
        ))
    })?;
    let mut bytes: Vec<u8> = Vec::new();
    std::io::BufReader::new(file)
        .take((PREVIEW_MAX_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| {
            Box::new(PreviewCache::placeholder(
                path,
                &format!("(cannot read: {e})"),
            ))
        })?;
    if bytes.len() > PREVIEW_MAX_BYTES {
        return Err(Box::new(PreviewCache::placeholder(
            path,
            "(file too large to preview)",
        )));
    }
    if bytes.iter().take(1024).any(|&b| b == 0) {
        return Err(Box::new(PreviewCache::placeholder(path, "(binary file)")));
    }
    String::from_utf8(bytes)
        .map_err(|_| Box::new(PreviewCache::placeholder(path, "(non-utf8 file)")))
}

/// Build a `PreviewCache` from in-memory text plus precomputed syntax spans.
/// `path` is used for display / cache key only — language routing happens
/// in the caller, since they own the per-language `SyntaxState` cache.
pub(crate) fn build_preview_from_text(
    path: &Path,
    source: &str,
    spans: Vec<HlSpan>,
) -> PreviewCache {
    if source.len() > PREVIEW_MAX_BYTES {
        return PreviewCache::placeholder(path, "(buffer too large to preview)");
    }
    // Manual line split: `str::lines` would also strip `\r`, but we'd lose the
    // ability to map line indices back to byte offsets the syntax spans use.
    let mut lines: Vec<String> = Vec::new();
    let mut line_byte_starts: Vec<usize> = vec![0];
    let src_bytes = source.as_bytes();
    let mut start = 0usize;
    for i in 0..src_bytes.len() {
        if src_bytes[i] == b'\n' {
            let end = if i > 0 && src_bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            lines.push(source[start..end].to_string());
            line_byte_starts.push(i + 1);
            start = i + 1;
        }
    }
    if start < src_bytes.len() {
        lines.push(source[start..].to_string());
    }
    line_byte_starts.push(src_bytes.len());

    PreviewCache {
        // Defaults to a path key; the Buffers builder overrides it with a
        // (idx, version) key before the cache is stored.
        key: PreviewKey::Path(path.to_path_buf()),
        path: path.to_path_buf(),
        lines,
        line_byte_starts,
        spans,
        placeholder: false,
    }
}

/// Compute syntax spans for a preview, routing through the per-language
/// `SyntaxState` cache so the query compile is one-time per language.
pub(crate) fn preview_spans(ed: &mut Editor, path: &Path, source: &str) -> Vec<HlSpan> {
    let Some(lang) = Language::from_path(path) else {
        return Vec::new();
    };
    let Some(state) = ed.cached_syntax(lang) else {
        return Vec::new();
    };
    state.highlight(source.as_bytes()).unwrap_or_default()
}

/// Keep the picker's preview MRU current for the highlighted row. Called
/// once per main-loop tick (not from the render path). A cache hit promotes
/// the entry to the front for free; a miss builds the preview after the
/// selection has been stable for `PREVIEW_DEBOUNCE_MS` (so j/k/scroll spam
/// doesn't read + parse a file per move), pushes it to the front, and evicts
/// the tail past `PREVIEW_LRU_CAP`. The renderer only reads `previews.first()`.
pub(crate) fn refresh_preview(ed: &mut Editor) {
    let kind = match ed.picker.as_ref().map(|p| p.kind.clone()) {
        Some(k) => k,
        None => return,
    };
    // Cheap guard: only a fullscreen kind with a preview pane does any work.
    if !(matches!(kind.spec().layout, PickerLayout::Full) && kind.spec().has_preview) {
        return;
    }

    // Resolve the key of the current selection's target. No target (empty
    // match list, non-previewable row) → leave the MRU untouched; the
    // renderer gates the pane on there being a valid selection.
    let Some(target) = current_preview_key(ed, &kind) else {
        return;
    };

    // MRU hit: promote to the front. Instant — no debounce, no I/O. Reset the
    // stability tracker so a later move to an *uncached* row restarts the
    // debounce timer from now.
    if let Some(p) = ed.picker.as_mut() {
        if let Some(pos) = p.previews.iter().position(|c| c.key == target) {
            if pos != 0 {
                let c = p.previews.remove(pos);
                p.previews.insert(0, c);
            }
            p.preview_last_seen_selected = Some(p.selected);
            return;
        }
    }

    // Miss. Track selection movement and debounce the rebuild: hold off while
    // the user is still moving, unless the MRU is empty (first preview builds
    // immediately so the pane isn't blank for 50 ms on open).
    let (empty, stable) = if let Some(p) = ed.picker.as_mut() {
        if p.preview_last_seen_selected != Some(p.selected) {
            p.preview_last_seen_selected = Some(p.selected);
            p.preview_changed_at = Instant::now();
        }
        let stable = Instant::now().duration_since(p.preview_changed_at)
            >= Duration::from_millis(PREVIEW_DEBOUNCE_MS);
        (p.previews.is_empty(), stable)
    } else {
        return;
    };
    if !empty && !stable {
        return;
    }

    let cache = match &target {
        PreviewKey::Path(path) => match read_preview_source(path) {
            Ok(source) => {
                let spans = preview_spans(ed, path, &source);
                build_preview_from_text(path, &source, spans)
            }
            Err(placeholder) => *placeholder,
        },
        PreviewKey::Buffer { idx, .. } => {
            // The rope view is what the user wants — it reflects unsaved edits.
            let mut c = build_preview_for_buffer_idx(ed, *idx);
            c.key = target.clone();
            c
        }
    };
    if let Some(p) = ed.picker.as_mut() {
        p.previews.insert(0, cache);
        p.previews.truncate(PREVIEW_LRU_CAP);
    }
}

/// The MRU key for the picker's currently-highlighted row, or `None` when
/// there's no selection or the row isn't previewable.
fn current_preview_key(ed: &Editor, kind: &PickerKind) -> Option<PreviewKey> {
    let p = ed.picker.as_ref()?;
    let &(item_idx, _) = p.matches.get(p.selected)?;
    match kind {
        PickerKind::Files | PickerKind::Grep => match &p.active_items()[item_idx].value {
            PickerValue::File(path) => Some(PreviewKey::Path(path.clone())),
            PickerValue::GrepHit { path, .. } => Some(PreviewKey::Path(path.clone())),
            _ => None,
        },
        PickerKind::Buffers => {
            if let PickerValue::BufferIndex(idx) = p.active_items()[item_idx].value {
                let version = buffer_version(ed, idx);
                Some(PreviewKey::Buffer { idx, version })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Version counter of the buffer at `idx` (0 = active, 1.. = parked), or 0
/// if it no longer exists. Feeds the Buffers MRU key so an edit invalidates.
fn buffer_version(ed: &Editor, idx: usize) -> u64 {
    if idx == 0 {
        ed.buffer.version()
    } else {
        ed.other_buffers
            .get(idx - 1)
            .map(|b| b.buffer.version())
            .unwrap_or(0)
    }
}

/// Build a `PreviewCache` for the buffer at `idx` (0 = active, 1.. = parked).
/// Always rebuilds from the live rope so dirty changes show through. Falls
/// back to a synthetic display path for unnamed buffers so syntax routing
/// and the preview header still have *something* to show.
pub(crate) fn build_preview_for_buffer_idx(ed: &mut Editor, idx: usize) -> PreviewCache {
    // Pull the buffer's path + text under an immutable borrow first; release
    // the borrow before reaching for the (mutable) syntax cache.
    let (path, text): (PathBuf, String) = {
        let buf: &Buffer = if idx == 0 {
            &ed.buffer
        } else {
            match ed.other_buffers.get(idx - 1) {
                Some(b) => &b.buffer,
                None => {
                    return PreviewCache::placeholder(
                        Path::new("[gone]"),
                        "(buffer no longer exists)",
                    )
                }
            }
        };
        let path = buf
            .path()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("[No Name]"));
        (path, buf.rope().to_string())
    };
    let spans = preview_spans(ed, &path, &text);
    build_preview_from_text(&path, &text, spans)
}
