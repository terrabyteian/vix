use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::HashMap;

use vix_core::{compile_search, find_all_in_lines, Case, Mode};
use vix_lsp::lsp_types::DiagnosticSeverity;
use vix_syntax::HlSpan;

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
            lines.push(Line::from(Span::styled("~", theme_tilde)));
            continue;
        }

        let num = format!("{:>width$} ", line_idx + 1, width = gutter_width - 1);
        let mut spans = vec![Span::styled(num, theme_gutter)];
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
                let style = match ed.theme.scope_styles.get(span.scope).copied().flatten() {
                    Some(s) => s,
                    None => continue,
                };
                for slot in styles.iter_mut().take(e_col).skip(s_col) {
                    *slot = Some(style);
                }
            }
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
