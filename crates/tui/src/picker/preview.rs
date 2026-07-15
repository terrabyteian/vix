//! Picker preview machinery: reading/caching file or buffer contents and
//! syntax-highlighting them for the fullscreen picker's preview pane.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vix_core::Buffer;
use vix_syntax::{HlSpan, Language, SyntaxState};

use crate::picker::{is_fullscreen_picker_kind, PickerKind, PickerValue, PreviewCache};
use crate::picker::{PREVIEW_DEBOUNCE_MS, PREVIEW_MAX_BYTES};
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
pub(crate) fn read_preview_source(path: &Path) -> Result<String, PreviewCache> {
    use std::io::Read;
    let file = std::fs::File::open(path)
        .map_err(|e| PreviewCache::placeholder(path, &format!("(cannot read: {e})")))?;
    let mut bytes: Vec<u8> = Vec::new();
    std::io::BufReader::new(file)
        .take((PREVIEW_MAX_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| PreviewCache::placeholder(path, &format!("(cannot read: {e})")))?;
    if bytes.len() > PREVIEW_MAX_BYTES {
        return Err(PreviewCache::placeholder(
            path,
            "(file too large to preview)",
        ));
    }
    if bytes.iter().take(1024).any(|&b| b == 0) {
        return Err(PreviewCache::placeholder(path, "(binary file)"));
    }
    String::from_utf8(bytes).map_err(|_| PreviewCache::placeholder(path, "(non-utf8 file)"))
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

/// If the picker is showing a fullscreen-kind selection whose target differs
/// from the cached preview, rebuild the cache. Cheap when the selection
/// hasn't moved; debounced when the user is actively scrolling so the
/// previous preview keeps showing instead of a render-time stall.
pub(crate) fn refresh_preview(ed: &mut Editor) {
    let kind = match ed.picker.as_ref().map(|p| p.kind.clone()) {
        Some(k) => k,
        None => return,
    };
    if !is_fullscreen_picker_kind(&kind) {
        if let Some(p) = ed.picker.as_mut() {
            p.preview = None;
        }
        return;
    }

    // Track selection movement. When `selected` differs from what we
    // observed last refresh, reset the stability timer; the rebuild gates
    // below will keep the existing preview visible until the timer expires.
    // The first preview (before any cache exists) is allowed through
    // immediately so the pane isn't blank for 50 ms on picker open.
    let (has_cache, stable) = if let Some(p) = ed.picker.as_mut() {
        if p.preview_last_seen_selected != Some(p.selected) {
            p.preview_last_seen_selected = Some(p.selected);
            p.preview_changed_at = Instant::now();
        }
        let stable = Instant::now().duration_since(p.preview_changed_at)
            >= Duration::from_millis(PREVIEW_DEBOUNCE_MS);
        (p.preview.is_some(), stable)
    } else {
        return;
    };

    match kind {
        PickerKind::Files | PickerKind::Grep => {
            let target_path: Option<PathBuf> = ed.picker.as_ref().and_then(|p| {
                p.matches
                    .get(p.selected)
                    .and_then(|&(idx, _)| match &p.items[idx].value {
                        PickerValue::File(path) => Some(path.clone()),
                        PickerValue::GrepHit { path, .. } => Some(path.clone()),
                        _ => None,
                    })
            });
            let Some(target) = target_path else {
                if let Some(p) = ed.picker.as_mut() {
                    p.preview = None;
                }
                return;
            };
            if let Some(p) = ed.picker.as_ref() {
                if p.preview.as_ref().is_some_and(|c| c.path == target) {
                    return;
                }
            }
            // Cache miss → would rebuild. Hold off if a previous preview is
            // still on screen and the user hasn't paused yet.
            if has_cache && !stable {
                return;
            }
            let cache = match read_preview_source(&target) {
                Ok(source) => {
                    let spans = preview_spans(ed, &target, &source);
                    build_preview_from_text(&target, &source, spans)
                }
                Err(placeholder) => placeholder,
            };
            if let Some(p) = ed.picker.as_mut() {
                p.preview = Some(cache);
            }
        }
        PickerKind::Buffers => {
            // Resolve the highlighted buffer index, release the &mut p
            // borrow, then read the buffer and rebuild the cache from the
            // rope. The rope view is what the user actually wants — it
            // reflects unsaved edits.
            let buffer_idx: Option<usize> = ed
                .picker
                .as_ref()
                .and_then(|p| p.matches.get(p.selected).map(|&(i, _)| i))
                .and_then(|item_idx| {
                    let p = ed.picker.as_ref()?;
                    if let PickerValue::BufferIndex(idx) = p.items[item_idx].value {
                        Some(idx)
                    } else {
                        None
                    }
                });
            let Some(idx) = buffer_idx else {
                if let Some(p) = ed.picker.as_mut() {
                    p.preview = None;
                }
                return;
            };
            if has_cache && !stable {
                return;
            }
            let cache = build_preview_for_buffer_idx(ed, idx);
            if let Some(p) = ed.picker.as_mut() {
                p.preview = Some(cache);
            }
        }
        _ => {
            if let Some(p) = ed.picker.as_mut() {
                p.preview = None;
            }
        }
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
