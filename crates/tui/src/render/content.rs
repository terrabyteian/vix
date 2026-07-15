use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::HashMap;
use std::fmt::Write as _;

use vix_core::{compile_search, find_all_in_lines, Case, Mode};
use vix_lsp::lsp_types::DiagnosticSeverity;
use vix_syntax::HlSpan;

use crate::editor::SearchHlCache;
use crate::lsp::sev_rank;
use crate::Editor;

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

    // Theme values used inside the per-row loop below. Copied out up front
    // (Style is Copy) so the loop's read-only borrows of `ed` don't have to
    // fight a live `&ed.theme.field` borrow at each use site.
    let theme_tilde = ed.theme.tilde;
    let theme_gutter = ed.theme.gutter;
    let theme_diag_error = ed.theme.diag_error;
    let theme_diag_warn = ed.theme.diag_warn;
    let theme_diag_info = ed.theme.diag_info;
    let theme_diag_hint = ed.theme.diag_hint;
    let theme_diag_none = ed.theme.diag_none;
    let theme_search_hl = ed.theme.search_hl;
    let theme_selection = ed.theme.selection;
    let theme_yank_flash = ed.theme.yank_flash;
    let theme_cursor_normal = ed.theme.cursor_normal;
    let theme_cursor_insert = ed.theme.cursor_insert;

    // Per-line diagnostic severity map for the active buffer. Cached on `ed`
    // keyed by (diagnostics_gen, path) — rebuilding a HashMap from the raw
    // diagnostic list is skipped entirely on frames where neither changed
    // (e.g. pure cursor movement between LSP publishes).
    let diag_by_line: HashMap<usize, DiagnosticSeverity> = match ed.buffer.path() {
        Some(path) => {
            let reuse = ed
                .diag_line_cache
                .as_ref()
                .map(|(gen, p, _)| *gen == ed.diagnostics_gen && p == path)
                .unwrap_or(false);
            if !reuse {
                let mut m: HashMap<usize, DiagnosticSeverity> = HashMap::new();
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
                ed.diag_line_cache = Some((ed.diagnostics_gen, path.to_path_buf(), m));
            }
            ed.diag_line_cache
                .as_ref()
                .map(|(_, _, m)| m.clone())
                .unwrap_or_default()
        }
        None => HashMap::new(),
    };

    // Compute search highlight ranges for the visible window. Cached on `ed`
    // keyed by (query, buffer id + version, viewport) so an unchanged frame
    // reuses the previous compile + scan instead of redoing both every draw.
    let highlights: Vec<(usize, usize)> = if ed.hl_search {
        if let Some((q, _)) = ed.last_search.clone() {
            let buffer_id = ed.active_bid;
            let buffer_version = ed.buffer.version();
            let view_top = ed.view_top;
            let reuse = ed.search_hl_cache.as_ref().is_some_and(|c| {
                c.query == q
                    && c.buffer_id == buffer_id
                    && c.buffer_version == buffer_version
                    && c.view_top == view_top
                    && c.rows == rows
            });
            if !reuse {
                match compile_search(&q, Case::Smart) {
                    Ok(re) => {
                        let ranges = find_all_in_lines(&ed.buffer, &re, view_top, view_top + rows);
                        ed.search_hl_cache = Some(SearchHlCache {
                            query: q,
                            buffer_id,
                            buffer_version,
                            view_top,
                            rows,
                            ranges,
                            re,
                        });
                    }
                    Err(_) => ed.search_hl_cache = None,
                }
            }
            ed.search_hl_cache
                .as_ref()
                .map(|c| c.ranges.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Windowed syntax spans: `hl_spans` covers the *whole file* (byte-sorted,
    // non-overlapping — guaranteed by vix-syntax), but only the visible rows
    // need styling. Narrow to the viewport's byte range once per frame
    // instead of scanning every span for every visible line.
    let view_bottom = (ed.view_top + rows).min(total_lines);
    let rope = ed.buffer.rope();
    let win_start = rope.char_to_byte(ed.buffer.line_to_char(ed.view_top));
    // `view_bottom == total_lines` means the viewport runs off the end of
    // the file — there's no "next line" to take a start-byte from, so the
    // window extends to the end of the buffer. Otherwise it's the byte
    // offset of the first line *past* the last visible one (still
    // half-open, matching the `lo..hi` slice below).
    let win_end = if view_bottom >= total_lines {
        rope.len_bytes()
    } else {
        rope.char_to_byte(ed.buffer.line_to_char(view_bottom))
    };
    let lo = hl_spans.partition_point(|s| s.range.end <= win_start);
    let hi = hl_spans.partition_point(|s| s.range.start < win_end);
    let windowed_spans = &hl_spans[lo..hi];
    // Monotone cursor into `windowed_spans`, advanced (never rewound) as the
    // row loop walks down the viewport in increasing byte order.
    let mut span_cursor = 0usize;

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    // Per-row scratch, reused across the loop instead of allocated fresh
    // each iteration.
    let mut num_buf = String::new();
    let mut chars: Vec<char> = Vec::new();
    let mut styles: Vec<Option<Style>> = Vec::new();
    for screen_row in 0..rows {
        let line_idx = ed.view_top + screen_row;
        if line_idx >= total_lines {
            lines.push(Line::from(Span::styled("~", theme_tilde)));
            continue;
        }

        num_buf.clear();
        let _ = write!(
            num_buf,
            "{:>width$} ",
            line_idx + 1,
            width = gutter_width - 1
        );
        let mut spans = vec![Span::styled(num_buf.clone(), theme_gutter)];
        // Diagnostic indicator column (one char).
        let (glyph, style) = match diag_by_line.get(&line_idx) {
            Some(&DiagnosticSeverity::ERROR) => ("●", theme_diag_error),
            Some(&DiagnosticSeverity::WARNING) => ("●", theme_diag_warn),
            Some(&DiagnosticSeverity::INFORMATION) => ("●", theme_diag_info),
            Some(&DiagnosticSeverity::HINT) => ("○", theme_diag_hint),
            Some(_) | None => (" ", theme_diag_none),
        };
        spans.push(Span::styled(glyph.to_string(), style));
        spans.push(Span::raw(" "));

        let line_start_char = ed.buffer.line_to_char(line_idx);
        let rope_line = rope.line(line_idx);
        let mut line_byte_len = rope_line.len_bytes();
        chars.clear();
        chars.extend(rope_line.chars());
        if chars.last() == Some(&'\n') {
            chars.pop();
            line_byte_len -= 1;
        }

        // Build per-char style overlay. Syntax highlighting is the base
        // layer; search/visual/cursor overlays override it.
        styles.clear();
        styles.resize(chars.len(), None);

        // Apply syntax spans overlapping this line. Spans use byte offsets;
        // convert them to per-line char columns via the rope.
        if !windowed_spans.is_empty() && !chars.is_empty() {
            let line_start_byte = rope.char_to_byte(line_start_char);
            let line = LineByteRange {
                start_char: line_start_char,
                start_byte: line_start_byte,
                end_byte: line_start_byte + line_byte_len,
            };
            apply_line_spans(
                windowed_spans,
                &mut span_cursor,
                rope,
                line,
                &ed.theme.scope_styles,
                &mut styles,
            );
        }

        // Apply search highlights that overlap this line.
        let hl_style = theme_search_hl;
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
            let sel_style = theme_selection;
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
            let flash_style = theme_yank_flash;
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
                    Mode::Insert => theme_cursor_insert,
                    _ => theme_cursor_normal,
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
                Mode::Insert => theme_cursor_insert,
                _ => theme_cursor_normal,
            };
            spans.push(Span::styled(" ", cursor_style));
        }

        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// A line's position expressed in both coordinate systems `apply_line_spans`
/// needs: `start_char` to turn span byte-offsets into per-line char columns,
/// `start_byte`/`end_byte` (half-open) to test span overlap. Bundled into one
/// struct purely to keep `apply_line_spans` under clippy's arg-count limit.
pub(crate) struct LineByteRange {
    pub(crate) start_char: usize,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
}

/// Applies syntax-highlight spans overlapping `line`'s byte range onto
/// `styles` (indexed by char column relative to the line, i.e.
/// `styles[col]` corresponds to byte-offset-derived char
/// `line.start_char + col`).
///
/// `spans` must be sorted by `range.start` and non-overlapping (guaranteed
/// by vix-syntax's highlighter). Iteration starts at `*cursor` and the
/// cursor is advanced past any span that ends at or before
/// `line.start_byte` — such a span can never matter again, since later
/// calls (by convention, for lines further down the buffer) only ever pass a
/// larger `start_byte`. Spans that start at or after `line.end_byte` are
/// left in place (not consumed) since they may still overlap a later line.
///
/// This is the single source of truth for "spans overlapping a line" used
/// by both the windowed (viewport-only slice) and brute-force (whole-file
/// slice) paths in `render_content` and its property test, so the two are
/// guaranteed to agree by construction whenever given equivalent inputs.
pub(crate) fn apply_line_spans(
    spans: &[HlSpan],
    cursor: &mut usize,
    rope: &ropey::Rope,
    line: LineByteRange,
    scope_styles: &[Option<Style>],
    styles: &mut [Option<Style>],
) {
    while *cursor < spans.len() && spans[*cursor].range.end <= line.start_byte {
        *cursor += 1;
    }
    let mut k = *cursor;
    while k < spans.len() && spans[k].range.start < line.end_byte {
        let span = &spans[k];
        if span.range.end > line.start_byte {
            let s_byte = span.range.start.max(line.start_byte);
            let e_byte = span.range.end.min(line.end_byte);
            let s_col = rope.byte_to_char(s_byte) - line.start_char;
            let e_col = rope.byte_to_char(e_byte) - line.start_char;
            if let Some(style) = scope_styles.get(span.scope).copied().flatten() {
                for slot in styles.iter_mut().take(e_col).skip(s_col) {
                    *slot = Some(style);
                }
            }
        }
        k += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Harness;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop(); // crates/
        p.pop(); // workspace root
        p.push("tests/fixtures");
        p.push(name);
        p
    }

    /// The windowed syntax-span path must produce byte-for-byte identical
    /// per-line styles to a brute-force scan over the *entire* span list,
    /// for every line in several different viewports. This is what protects
    /// `render_content`'s windowing (§1) from silently dropping or
    /// misapplying a span at the viewport's edges.
    #[test]
    fn windowed_spans_match_brute_force_scan() {
        let path = fixture("sample.rs");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut h = Harness::with_text_and_path(&text, path);
        h.editor.refresh_syntax_cache();
        let hl_spans = h.editor.syntax_cache.clone();
        assert!(!hl_spans.is_empty(), "fixture should produce syntax spans");

        let rope = h.editor.buffer.rope().clone();
        let total_lines = h.editor.buffer.len_lines();
        let scope_styles = h.editor.theme.scope_styles.clone();

        // (view_top, rows) combinations exercising: start of file, a middle
        // slice, a single line, a viewport that overhangs past EOF, and one
        // covering the whole file (rows == total_lines).
        let combos: &[(usize, usize)] = &[
            (0, 5),
            (2, 3),
            (5, 1),
            (0, total_lines),
            (total_lines.saturating_sub(2), 5),
            (3, total_lines),
        ];

        for &(view_top, rows) in combos {
            let view_bottom = (view_top + rows).min(total_lines);
            let win_start = rope.char_to_byte(h.editor.buffer.line_to_char(view_top));
            let win_end = if view_bottom >= total_lines {
                rope.len_bytes()
            } else {
                rope.char_to_byte(h.editor.buffer.line_to_char(view_bottom))
            };
            let lo = hl_spans.partition_point(|s| s.range.end <= win_start);
            let hi = hl_spans.partition_point(|s| s.range.start < win_end);
            let windowed = &hl_spans[lo..hi];

            let mut windowed_cursor = 0usize;
            let mut brute_cursor = 0usize;
            for line_idx in view_top..view_bottom {
                let line_start_char = h.editor.buffer.line_to_char(line_idx);
                let rope_line = rope.line(line_idx);
                let mut line_byte_len = rope_line.len_bytes();
                let mut chars: Vec<char> = rope_line.chars().collect();
                if chars.last() == Some(&'\n') {
                    chars.pop();
                    line_byte_len -= 1;
                }
                let line_start_byte = rope.char_to_byte(line_start_char);
                let line_end_byte = line_start_byte + line_byte_len;

                let mut windowed_styles: Vec<Option<Style>> = vec![None; chars.len()];
                apply_line_spans(
                    windowed,
                    &mut windowed_cursor,
                    &rope,
                    LineByteRange {
                        start_char: line_start_char,
                        start_byte: line_start_byte,
                        end_byte: line_end_byte,
                    },
                    &scope_styles,
                    &mut windowed_styles,
                );

                let mut brute_styles: Vec<Option<Style>> = vec![None; chars.len()];
                apply_line_spans(
                    &hl_spans,
                    &mut brute_cursor,
                    &rope,
                    LineByteRange {
                        start_char: line_start_char,
                        start_byte: line_start_byte,
                        end_byte: line_end_byte,
                    },
                    &scope_styles,
                    &mut brute_styles,
                );

                assert_eq!(
                    windowed_styles, brute_styles,
                    "style mismatch at line {line_idx} for view_top={view_top} rows={rows}"
                );
            }
        }
    }
}
