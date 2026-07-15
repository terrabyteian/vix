use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::HashMap;

use vix_core::{compile_search, find_all_in_lines, Case, Mode};
use vix_lsp::lsp_types::DiagnosticSeverity;
use vix_syntax::{HlSpan, HIGHLIGHT_NAMES};

use crate::lsp::sev_rank;
use crate::Editor;

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
