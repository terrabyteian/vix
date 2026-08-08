//! Picker preview machinery: reading/caching file or buffer contents and
//! syntax-highlighting them for the picker preview panes (the Buffers
//! fullscreen split and the omnibox).
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vix_core::Buffer;
use vix_syntax::{HlSpan, Language, SyntaxState};

use crate::picker::{omni_key_path, PickerKind, PickerValue, PreviewCache, PreviewKey};
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

/// Build a `PreviewCache` from in-memory text plus precomputed syntax spans.
/// `path` is used for display / cache key only — language routing happens
/// in the caller, since they own the per-language `SyntaxState` cache.
pub(crate) fn build_preview_from_text(
    path: &Path,
    source: &str,
    mut spans: Vec<HlSpan>,
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

    // Spans arrive in ascending byte order from tree-sitter's event stream
    // and never overlap; the renderer binary-searches on that invariant.
    // Sort defensively — one-time O(n log n) at build, free when already sorted.
    spans.sort_by_key(|s| s.range.start);

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
    // Cheap guard: only kinds with a preview pane do any work.
    if !kind.spec().has_preview {
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
            let promoted = pos != 0;
            if promoted {
                let c = p.previews.remove(pos);
                p.previews.insert(0, c);
            }
            p.preview_last_seen_selected = Some(p.selected);
            if promoted {
                // The pane now shows a different cached file.
                ed.request_redraw();
            }
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
        // Omni rows: read the file from disk. The key path is already
        // normalized (root-relative when possible) by `omni_key_path`.
        PreviewKey::Path(path) => {
            let mut c = build_preview_for_path(ed, path);
            c.key = target.clone();
            c
        }
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
    // A fresh preview was built for the pane.
    ed.request_redraw();
}

/// When the preview pane will need a timer wake-up: a rebuild is owed (the
/// highlighted row's target isn't the MRU front) and is being held by the
/// debounce. `None` when the pane is current or no preview picker is open.
/// Derived entirely from state so the main loop can't strand a pending
/// rebuild by missing a stored deadline.
pub(crate) fn preview_wake_deadline(ed: &Editor) -> Option<Instant> {
    let p = ed.picker.as_ref()?;
    if !p.kind.spec().has_preview {
        return None;
    }
    let target = current_preview_key(ed, &p.kind)?;
    if p.previews.first().is_some_and(|c| c.key == target) {
        return None;
    }
    Some(p.preview_changed_at + Duration::from_millis(PREVIEW_DEBOUNCE_MS))
}

/// The MRU key for the picker's currently-highlighted row, or `None` when
/// there's no selection or the row isn't previewable.
fn current_preview_key(ed: &Editor, kind: &PickerKind) -> Option<PreviewKey> {
    let p = ed.picker.as_ref()?;
    let &(r, _) = p.matches.get(p.selected)?;
    match kind {
        PickerKind::Buffers => {
            if let PickerValue::BufferIndex(idx) = p.item(r).value {
                let version = buffer_version(ed, idx);
                Some(PreviewKey::Buffer { idx, version })
            } else {
                None
            }
        }
        // Omni rows key on the normalized file path. The grep LINE is
        // deliberately not part of the key — every hit in the same file
        // shares one cache entry; centering on the hit line is a
        // render-time concern.
        PickerKind::Omni => match &p.item(r).value {
            PickerValue::File(path) | PickerValue::GrepHit { path, .. } => {
                Some(PreviewKey::Path(omni_key_path(&ed.root, path)))
            }
            _ => None,
        },
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

/// Build a preview by reading `path` from disk (unlike Buffers previews,
/// which read the live rope). Size-gated before the read, binary-gated
/// after, lossy-UTF-8. Whole-file highlight — `highlight_range` is the
/// follow-up if preview parses ever show up as jank.
///
/// `path` is the normalized (possibly root-relative) key path and stays as
/// given in the returned cache — it is what the pane header and the MRU key
/// display. The *read* goes through `resolve_in_root`, so a relative key
/// resolves against the omni scan root rather than the process cwd.
fn build_preview_for_path(ed: &mut Editor, path: &Path) -> PreviewCache {
    let abs = ed.resolve_in_root(path);
    if let Ok(meta) = fs::metadata(&abs) {
        if meta.len() > PREVIEW_MAX_BYTES as u64 {
            return PreviewCache::placeholder(path, "(file too large to preview)");
        }
    }
    let bytes = match fs::read(&abs) {
        Ok(b) => b,
        Err(_) => return PreviewCache::placeholder(path, "(unable to read file)"),
    };
    // A NUL in the head is the cheap classic binary sniff.
    if bytes.iter().take(1024).any(|&b| b == 0) {
        return PreviewCache::placeholder(path, "(binary file)");
    }
    let text = String::from_utf8_lossy(&bytes);
    let spans = preview_spans(ed, path, &text);
    build_preview_from_text(path, &text, spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::{ItemRef, Picker, PickerItem};
    use crate::testing::Harness;
    use vix_picker::Utf32String;

    /// Unique scratch dir per test so parallel test runs (same pid) never
    /// collide, mirroring `recent.rs`'s temp-dir tests.
    fn test_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "vix-preview-test-{}-{name}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn file_item(path: &Path) -> PickerItem {
        let d = path.to_string_lossy().into_owned();
        PickerItem {
            display: d.clone(),
            value: PickerValue::File(path.to_path_buf()),
            haystack: Utf32String::from(d.as_str()),
        }
    }

    fn grep_item(path: &Path, line: u64) -> PickerItem {
        let d = path.to_string_lossy().into_owned();
        PickerItem {
            display: d.clone(),
            value: PickerValue::GrepHit {
                path: path.to_path_buf(),
                line,
            },
            haystack: Utf32String::from(d.as_str()),
        }
    }

    #[test]
    fn omni_key_path_normalizes_absolute_to_relative() {
        let root = Path::new("/projects/demo");
        let rel = Path::new("src/foo.rs");
        assert_eq!(
            omni_key_path(root, &root.join(rel)),
            omni_key_path(root, rel)
        );
        assert_eq!(omni_key_path(root, rel), rel);
    }

    #[test]
    fn omni_file_and_grep_rows_share_a_preview_key() {
        let root = test_dir("shared-key");
        let mut ed = Editor::new(Buffer::empty());
        ed.set_root(root.clone());
        // Canonicalized on the way in (macOS temp dirs are symlinked), so
        // absolute grep paths have to be built from the stored root.
        let root = ed.root.clone();
        // A File row with a relative path plus two GrepHit rows for the SAME
        // file at an absolute path and different lines: all three selections
        // must resolve to one cache key.
        let mut p = Picker::new(PickerKind::Omni, Vec::new());
        p.file_items.push(file_item(Path::new("src/foo.rs")));
        p.grep_items.push(grep_item(&root.join("src/foo.rs"), 3));
        p.grep_items.push(grep_item(&root.join("src/foo.rs"), 9));
        p.matches = vec![
            (ItemRef::File(0), 0),
            (ItemRef::Grep(0), 0),
            (ItemRef::Grep(1), 0),
        ];
        ed.picker = Some(p);

        let key_at = |ed: &mut Editor, sel: usize| {
            ed.picker.as_mut().unwrap().selected = sel;
            current_preview_key(ed, &PickerKind::Omni).expect("previewable row")
        };
        let file_key = key_at(&mut ed, 0);
        assert_eq!(file_key, PreviewKey::Path(PathBuf::from("src/foo.rs")));
        assert_eq!(key_at(&mut ed, 1), file_key, "grep hit shares the file key");
        assert_eq!(
            key_at(&mut ed, 2),
            file_key,
            "the hit line is not part of the key"
        );
    }

    #[test]
    fn build_preview_for_path_reads_file() {
        let dir = test_dir("reads");
        let path = dir.join("sample.rs");
        fs::write(&path, "fn sample() {}\nsecond line\n").unwrap();
        let mut ed = Editor::new(Buffer::empty());
        let c = build_preview_for_path(&mut ed, &path);
        let _ = fs::remove_dir_all(&dir);
        assert!(!c.placeholder);
        assert_eq!(c.key, PreviewKey::Path(path.clone()));
        assert_eq!(c.path, path);
        assert_eq!(c.lines, vec!["fn sample() {}", "second line"]);
    }

    #[test]
    fn build_preview_for_path_oversize_placeholder() {
        let dir = test_dir("oversize");
        let path = dir.join("big.txt");
        fs::write(&path, vec![b'a'; PREVIEW_MAX_BYTES + 1]).unwrap();
        let mut ed = Editor::new(Buffer::empty());
        let c = build_preview_for_path(&mut ed, &path);
        let _ = fs::remove_dir_all(&dir);
        assert!(c.placeholder);
        assert_eq!(c.lines, vec!["(file too large to preview)"]);
    }

    #[test]
    fn build_preview_for_path_binary_placeholder() {
        let dir = test_dir("binary");
        let path = dir.join("blob.bin");
        fs::write(&path, b"\x7fELF\x00\x01\x02rest").unwrap();
        let mut ed = Editor::new(Buffer::empty());
        let c = build_preview_for_path(&mut ed, &path);
        let _ = fs::remove_dir_all(&dir);
        assert!(c.placeholder);
        assert_eq!(c.lines, vec!["(binary file)"]);
    }

    #[test]
    fn build_preview_for_path_missing_placeholder() {
        let dir = test_dir("missing");
        let path = dir.join("no_such_file.rs");
        let mut ed = Editor::new(Buffer::empty());
        let c = build_preview_for_path(&mut ed, &path);
        let _ = fs::remove_dir_all(&dir);
        assert!(c.placeholder);
        assert_eq!(c.lines, vec!["(unable to read file)"]);
    }

    #[test]
    fn omni_selection_builds_a_disk_preview() {
        // End-to-end: open the omnibox over a scratch repo, land the
        // selection on the file row, and refresh. The MRU is empty, so the
        // first build skips the debounce and the pane fills immediately.
        let dir = test_dir("omni-e2e");
        fs::write(dir.join("alpha.rs"), "fn alpha() {}\n").unwrap();
        let mut h = Harness::new();
        h.set_root(&dir);
        h.editor.open_files_picker();
        h.pump_picker();
        refresh_preview(&mut h.editor);
        let p = h.editor.picker.as_ref().expect("picker open");
        let c = p
            .previews
            .first()
            .expect("first preview builds immediately when the MRU is empty");
        assert!(!c.placeholder);
        assert_eq!(c.path, Path::new("alpha.rs"));
        assert_eq!(c.key, PreviewKey::Path(PathBuf::from("alpha.rs")));
        assert!(c.lines.iter().any(|l| l.contains("fn alpha")));
    }
}
