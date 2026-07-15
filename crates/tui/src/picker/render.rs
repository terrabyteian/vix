//! Picker rendering: the centered overlay for small pickers and the
//! fullscreen split-pane layout (list + preview) for Files/Grep/Buffers.
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::picker::preview::refresh_preview;
use crate::picker::{
    fit_picker_row, substring_match_smart, wrap_picker_detail, Picker, PickerItem, PickerKind,
    PickerLayout, PickerValue, PICKER_ACCENT, PICKER_ACCENT_HI, PICKER_BORDER, PICKER_DIM,
};
use crate::render::content::scope_style;
use crate::util::{char_index_in_byte_range, count_chars, pad_or_trunc, take_end};
use crate::Editor;

/// One-line key hint for the picker footer/header, shared between the
/// overlay and fullscreen renderers so the wording stays consistent. Gated
/// by kind: the Files↔Grep toggle only applies to those two kinds; marks
/// only to `supports_marks` kinds; buffer management only to `Buffers`.
pub(crate) fn picker_hint_line(
    kind: &PickerKind,
    supports_marks: bool,
    buffer_actions: bool,
) -> String {
    let toggle = match kind {
        PickerKind::Files => Some("<Tab> grep"),
        PickerKind::Grep => Some("<Tab> files"),
        _ => None,
    };
    let mut s = String::from("\u{2191}\u{2193}/C-j/C-k nav \u{00b7} <CR> open");
    if let Some(t) = toggle {
        s.push_str(" \u{00b7} ");
        s.push_str(t);
    }
    if supports_marks {
        s.push_str(" \u{00b7} C-Spc mark \u{00b7} A-c clear");
    }
    if buffer_actions {
        s.push_str(" \u{00b7} C-s save \u{00b7} C-q close \u{00b7} C-r reload");
    }
    s.push_str(" \u{00b7} <Esc> close");
    s
}

pub(crate) fn render_picker(f: &mut ratatui::Frame, area: Rect, ed: &mut Editor) {
    let Some(p) = ed.picker.as_ref() else {
        ed.last_picker_rect = None;
        ed.last_picker_list_rows = 0;
        return;
    };
    if matches!(p.kind.spec().layout, PickerLayout::Full) {
        render_picker_fullscreen(f, area, ed);
    } else {
        render_picker_overlay(f, area, ed);
    }
}

pub(crate) fn render_picker_overlay(f: &mut ratatui::Frame, area: Rect, ed: &mut Editor) {
    let Some(p) = ed.picker.as_ref() else {
        ed.last_picker_rect = None;
        ed.last_picker_list_rows = 0;
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

    let kind_label = p.kind.spec().label;
    let prompt = format!(" {} > {}", kind_label, p.query);
    let is_unified = p.kind.spec().supports_marks;
    let is_buffers = p.kind.spec().buffer_actions;
    let nav_hint = format!(" {} ", picker_hint_line(&p.kind, is_unified, is_buffers));
    let mark_hint = if is_unified && !p.marked.is_empty() {
        format!("[{}m] ", p.marked.len())
    } else {
        String::new()
    };
    let count = format!(
        "{}{}{}/{} ",
        nav_hint,
        mark_hint,
        p.matches.len(),
        p.active_items().len()
    );
    let header_pad = (w as usize).saturating_sub(prompt.len() + count.len());
    let header = Line::from(vec![
        Span::styled(
            prompt,
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ".repeat(header_pad), Style::default().bg(Color::DarkGray)),
        Span::styled(count, Style::default().bg(Color::DarkGray).fg(Color::Gray)),
    ]);

    let selected_item = p
        .matches
        .get(p.selected)
        .map(|&(item_idx, _)| &p.active_items()[item_idx]);
    let detail_rows = if selected_item.is_some() && h >= 8 {
        2usize.min((h as usize).saturating_sub(2))
    } else {
        0
    };
    let list_rows = (h as usize).saturating_sub(1 + detail_rows);
    // Scroll window so `selected` is visible.
    let scroll = if list_rows == 0 {
        0
    } else if p.selected >= list_rows {
        p.selected + 1 - list_rows
    } else {
        0
    };

    // Grep with a too-short query has no items; show a dim hint at the top
    // of the list instead of leaving it blank.
    let grep_hint = if matches!(p.kind, PickerKind::Grep)
        && p.query.chars().count() < p.kind.spec().min_query_len
    {
        Some(format!(
            "type {}+ characters to search",
            p.kind.spec().min_query_len
        ))
    } else {
        None
    };

    let mut lines: Vec<Line> = Vec::with_capacity(h as usize);
    lines.push(header);
    for row in 0..list_rows {
        let idx = scroll + row;
        match p.matches.get(idx) {
            Some(&(item_idx, _)) => {
                let item = &p.active_items()[item_idx];
                let is_sel = idx == p.selected;
                let style = if is_sel {
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    bg
                };
                // 2-char left gutter on Files/Grep so marks have somewhere
                // to render. Other pickers don't support multi-select, so
                // we leave their layout untouched.
                let (gutter, content_w) = if is_unified {
                    let g = if p.marked.contains(&item_idx) {
                        "● "
                    } else {
                        "  "
                    };
                    (g, (w as usize).saturating_sub(2))
                } else {
                    ("", w as usize)
                };
                let text = fit_picker_row(item, content_w);
                let row_text = format!("{gutter}{text}");
                let pad = (w as usize).saturating_sub(count_chars(&row_text));
                lines.push(Line::from(vec![
                    Span::styled(row_text, style),
                    Span::styled(" ".repeat(pad), style),
                ]));
            }
            None => {
                if row == 0 {
                    if let Some(hint) = &grep_hint {
                        lines.push(Line::from(Span::styled(
                            format!(" {hint}"),
                            Style::default().bg(Color::Black).fg(PICKER_DIM),
                        )));
                        continue;
                    }
                }
                lines.push(Line::raw(""));
            }
        }
    }
    if detail_rows > 0 {
        let detail_style = Style::default().bg(Color::DarkGray).fg(Color::White);
        let inner_w = (w as usize).saturating_sub(2);
        let detail = selected_item
            .map(|item| item.display.as_str())
            .unwrap_or("");
        let wrapped = wrap_picker_detail(detail, inner_w, detail_rows);
        for row in 0..detail_rows {
            let text = wrapped.get(row).map(String::as_str).unwrap_or("");
            let content = format!("  {text}");
            let pad = (w as usize).saturating_sub(count_chars(&content));
            lines.push(Line::from(vec![
                Span::styled(content, detail_style),
                Span::styled(" ".repeat(pad), detail_style),
            ]));
        }
    }
    while lines.len() < h as usize {
        lines.push(Line::raw(""));
    }
    f.render_widget(Paragraph::new(lines), overlay);

    ed.last_picker_rect = Some(overlay);
    ed.last_picker_scroll = scroll;
    ed.last_picker_list_rows = list_rows;
    if let Some(pm) = ed.picker.as_mut() {
        pm.last_list_rows = list_rows;
    }
}

// --- Fullscreen Files / Grep picker ----------------------------------------

/// Picker-time helper: the line number to anchor the preview around for the
/// currently-highlighted item. Files anchor at line 0; grep hits at the hit
/// line.
pub(crate) fn picker_preview_anchor_line(p: &Picker) -> usize {
    p.matches
        .get(p.selected)
        .and_then(|&(idx, _)| match &p.active_items()[idx].value {
            PickerValue::GrepHit { line, .. } => Some(line.saturating_sub(1) as usize),
            _ => None,
        })
        .unwrap_or(0)
}

/// Subtle two-step accent pulse driven by wall-clock time. The picker is the
/// only source of redraw cadence here, but the run loop polls every 100ms,
/// so colors change roughly twice per second when nothing else triggers a
/// redraw.
pub(crate) fn picker_pulse_accent() -> Color {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if (ms / 600) % 2 == 0 {
        PICKER_ACCENT
    } else {
        PICKER_ACCENT_HI
    }
}

pub(crate) fn render_picker_fullscreen(f: &mut ratatui::Frame, area: Rect, ed: &mut Editor) {
    // Tiny terminal — fall back to the overlay so we don't render garbage.
    if area.width < 50 || area.height < 12 {
        render_picker_overlay(f, area, ed);
        return;
    }

    refresh_preview(ed);

    // `&mut` (not `&ref`): the match-highlight pass below needs `&mut
    // p.scorer` alongside a borrow of the active item storage, which only
    // works if `p` itself is mutable (see the `active_items` binding before
    // the list loop).
    let Some(p) = ed.picker.as_mut() else {
        ed.last_picker_rect = None;
        ed.last_picker_list_rows = 0;
        return;
    };

    let w = area.width as usize;
    let h = area.height as usize;

    // --- Layout slots ---------------------------------------------------------
    // 0: tabs/breadcrumb · 1: prompt · 2: top sep · 3..h-2: body · h-2: bot sep
    // h-1: footer
    let body_top = 3u16;
    let body_bottom = (h as u16).saturating_sub(2);
    let body_h = body_bottom.saturating_sub(body_top) as usize;

    // Body split: ~38% list, rest preview, with one separator column.
    let split_enabled = w >= 80;
    let list_w = if split_enabled {
        ((w as u32 * 38 / 100)
            .max(28)
            .min((w as u32).saturating_sub(40))) as usize
    } else {
        w
    };
    let sep_col = list_w as u16;
    let preview_x = (list_w as u16) + 1;
    let preview_w = if split_enabled {
        w.saturating_sub(list_w + 1)
    } else {
        0
    };

    // Wipe the whole area to a clean Reset background. The editor view, status
    // line, and command line are not drawn under us when the fullscreen picker
    // is active (see `render`), so we own every cell here.
    let blank: Vec<Line> = (0..h).map(|_| Line::raw(" ".repeat(w))).collect();
    f.render_widget(Paragraph::new(blank), area);

    // --- Row 0: tabs + breadcrumb + counts -----------------------------------
    let is_buffers = p.kind.spec().buffer_actions;
    let is_unified = p.kind.spec().supports_marks;
    let mode_files = matches!(p.kind, PickerKind::Files);
    let cwd_str = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // Trim deep cwds — ".../<last 30>" — so wide screens don't blow out.
    let cwd_disp = if count_chars(&cwd_str) > 36 {
        format!("…{}", take_end(&cwd_str, 35))
    } else {
        cwd_str
    };
    let counts = format!(" {} / {} ", p.matches.len(), p.active_items().len());
    let bread = format!(" {}  ", cwd_disp);
    let active_style = Style::default()
        .fg(picker_pulse_accent())
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(PICKER_DIM);
    let tab_row = if is_buffers {
        // No Files↔Grep toggle here; show a single "Buffers" title.
        let title = " [Buffers] ";
        let title_used = count_chars(title);
        let right_used = count_chars(&counts) + count_chars(&bread);
        let middle_pad = w.saturating_sub(title_used + right_used);
        Line::from(vec![
            Span::styled(title, active_style),
            Span::raw(" ".repeat(middle_pad)),
            Span::styled(bread, Style::default().fg(PICKER_DIM)),
            Span::styled(
                counts,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        // " [Files]  Grep " or "  Files  [Grep] " — square brackets call out the active.
        let files_tab = if mode_files { " [Files] " } else { "  Files  " };
        let grep_tab = if mode_files { "  Grep  " } else { " [Grep] " };
        let tabs_used = count_chars(files_tab) + count_chars(grep_tab);
        let right_used = count_chars(&counts) + count_chars(&bread);
        let middle_pad = w.saturating_sub(tabs_used + right_used);
        Line::from(vec![
            Span::styled(
                files_tab,
                if mode_files {
                    active_style
                } else {
                    inactive_style
                },
            ),
            Span::styled(
                grep_tab,
                if mode_files {
                    inactive_style
                } else {
                    active_style
                },
            ),
            Span::raw(" ".repeat(middle_pad)),
            Span::styled(bread, Style::default().fg(PICKER_DIM)),
            Span::styled(
                counts,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    };
    f.render_widget(
        Paragraph::new(tab_row),
        Rect::new(area.x, area.y, area.width, 1),
    );

    // --- Row 1: prompt -------------------------------------------------------
    let arrow_style = Style::default()
        .fg(picker_pulse_accent())
        .add_modifier(Modifier::BOLD);
    // The picker is always in typing mode, so the caret always blinks.
    let caret = {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        if (ms / 500) % 2 == 0 {
            "▏"
        } else {
            " "
        }
    };
    let prompt_line = Line::from(vec![
        Span::raw(" "),
        Span::styled("❯ ", arrow_style),
        Span::styled(
            p.query.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(caret, Style::default().fg(picker_pulse_accent())),
    ]);
    f.render_widget(
        Paragraph::new(prompt_line),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );

    // --- Row 2: top separator ------------------------------------------------
    let sep = Line::from(Span::styled(
        "─".repeat(w),
        Style::default().fg(PICKER_BORDER),
    ));
    f.render_widget(
        Paragraph::new(sep.clone()),
        Rect::new(area.x, area.y + 2, area.width, 1),
    );

    // --- Body: list pane ------------------------------------------------------
    let selected = p.selected;
    let scroll = if body_h == 0 {
        0
    } else if selected >= body_h {
        selected + 1 - body_h
    } else {
        0
    };
    let mut list_lines: Vec<Line> = Vec::with_capacity(body_h);
    // Width budget for the row text after the gutter.
    // Layout per row: " ▶ ●  <text> "  where ▶ is selection, ● is mark
    // gutter. Reserve 4 cells for "▶  " + mark + 1 trailing space.
    let row_chrome = 4usize;
    let content_w = list_w.saturating_sub(row_chrome + 1);

    // Resolved once (kind is fixed for the whole render): a *direct* field
    // match rather than `p.active_items()`, so the borrow only ever covers
    // one of `file_items`/`grep_items`/`items` — leaving `p.scorer` free to
    // borrow mutably inside the loop below for the match-highlight pass.
    let active_items: &[PickerItem] = match p.kind {
        PickerKind::Files => &p.file_items,
        PickerKind::Grep => &p.grep_items,
        _ => &p.items,
    };

    // Grep with a too-short query has no items; show a dim hint at the top
    // of the list instead of leaving it blank.
    let grep_hint = if matches!(p.kind, PickerKind::Grep)
        && p.query.chars().count() < p.kind.spec().min_query_len
    {
        Some(format!(
            "type {}+ characters to search",
            p.kind.spec().min_query_len
        ))
    } else {
        None
    };

    for row in 0..body_h {
        let match_idx = scroll + row;
        let Some(&(item_idx, _)) = p.matches.get(match_idx) else {
            if row == 0 {
                if let Some(hint) = &grep_hint {
                    list_lines.push(Line::from(Span::styled(
                        format!("   {hint}"),
                        Style::default().fg(PICKER_DIM),
                    )));
                    continue;
                }
            }
            list_lines.push(Line::raw(""));
            continue;
        };
        let item = &active_items[item_idx];
        let is_sel = match_idx == selected;
        let is_marked = p.marked.contains(&item_idx);
        let row_style = if is_sel {
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let sel_glyph = if is_sel { "▶" } else { " " };
        let mark_glyph = if is_marked { "●" } else { " " };
        let displayed = fit_picker_row(item, content_w);

        // fzf-style match highlight: bold-yellow the chars in the row that
        // matched the live query. Only safe when the row wasn't truncated —
        // path/grep fitters don't expose the input→display char map. Grep
        // rows skip this entirely: their list display is just `path:line`
        // and the query is a regex over file *contents*, so fuzzy-matching
        // the path would highlight nothing useful and waste cycles.
        // Files uses substring (not fuzzy), so we highlight a single
        // contiguous run starting at the match position.
        let highlight_chars: std::collections::HashSet<usize> = if !p.query.is_empty()
            && displayed == item.display
            && !matches!(p.kind, PickerKind::Grep)
        {
            if matches!(p.kind, PickerKind::Files) {
                match substring_match_smart(&item.display, &p.query) {
                    Some(byte_off) => {
                        let char_start = item.display[..byte_off].chars().count();
                        let char_count = p.query.chars().count();
                        (char_start..char_start + char_count).collect()
                    }
                    None => std::collections::HashSet::new(),
                }
            } else {
                p.scorer
                    .match_indices(&item.haystack, &p.query)
                    .into_iter()
                    .map(|i| i as usize)
                    .collect()
            }
        } else {
            std::collections::HashSet::new()
        };

        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(format!(" {sel_glyph} "), row_style));
        spans.push(Span::styled(
            mark_glyph.to_string(),
            if is_marked && !is_sel {
                Style::default().fg(picker_pulse_accent())
            } else {
                row_style
            },
        ));
        spans.push(Span::styled(" ".to_string(), row_style));
        if highlight_chars.is_empty() {
            spans.push(Span::styled(displayed.clone(), row_style));
        } else {
            for (i, c) in displayed.chars().enumerate() {
                let style = if highlight_chars.contains(&i) {
                    if is_sel {
                        Style::default()
                            .bg(Color::Blue)
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    }
                } else {
                    row_style
                };
                spans.push(Span::styled(c.to_string(), style));
            }
        }
        // Pad row out to list width so the selection background covers it.
        let used = row_chrome + count_chars(&displayed);
        let pad = list_w.saturating_sub(used);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), row_style));
        }
        list_lines.push(Line::from(spans));
    }
    f.render_widget(
        Paragraph::new(list_lines),
        Rect::new(area.x, area.y + body_top, list_w as u16, body_h as u16),
    );

    // --- Body: vertical separator + preview pane -----------------------------
    if split_enabled {
        let sep_col_lines: Vec<Line> = (0..body_h)
            .map(|_| Line::from(Span::styled("│", Style::default().fg(PICKER_BORDER))))
            .collect();
        f.render_widget(
            Paragraph::new(sep_col_lines),
            Rect::new(area.x + sep_col, area.y + body_top, 1, body_h as u16),
        );

        render_picker_preview_pane(
            f,
            Rect::new(
                area.x + preview_x,
                area.y + body_top,
                preview_w as u16,
                body_h as u16,
            ),
            p,
        );
    }

    // --- Bottom separator -----------------------------------------------------
    f.render_widget(
        Paragraph::new(sep),
        Rect::new(area.x, area.y + body_bottom, area.width, 1),
    );

    // --- Footer hints --------------------------------------------------------
    let footer_body = format!(" {}", picker_hint_line(&p.kind, is_unified, is_buffers));
    let mark_status = if !p.marked.is_empty() {
        format!(" [{} marked] ", p.marked.len())
    } else {
        String::new()
    };
    let footer_pad = w.saturating_sub(count_chars(&footer_body) + count_chars(&mark_status));
    let footer_line = Line::from(vec![
        Span::styled(footer_body, Style::default().fg(PICKER_DIM)),
        Span::raw(" ".repeat(footer_pad)),
        Span::styled(
            mark_status,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(
        Paragraph::new(footer_line),
        Rect::new(area.x, area.y + (h as u16 - 1), area.width, 1),
    );

    // --- Mouse hit-test bookkeeping ------------------------------------------
    // `last_picker_rect` is the *list* area (not including chrome) so the
    // mouse handler can map row → match index without subtracting a header.
    ed.last_picker_rect = Some(Rect::new(
        area.x,
        area.y + body_top,
        list_w as u16,
        body_h as u16,
    ));
    ed.last_picker_scroll = scroll;
    ed.last_picker_list_rows = body_h;
    p.last_list_rows = body_h;
}

/// Draw the preview pane for the highlighted Files/Grep row. Caller positions
/// the rect; we own every cell inside it.
pub(crate) fn render_picker_preview_pane(f: &mut ratatui::Frame, area: Rect, p: &Picker) {
    let w = area.width as usize;
    let h = area.height as usize;
    if w < 8 || h < 3 {
        return;
    }

    let Some(cache) = p.preview.as_ref() else {
        // No selection → empty pane (paragraph would write spaces, but the
        // background wipe in render_picker_fullscreen already cleared us).
        return;
    };

    // Pane header: file name, dimmed, with a thin bottom rule.
    let header_text = pad_or_trunc(
        &format!(
            " {} ",
            cache
                .path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
        w,
    );
    let header_line = Line::from(Span::styled(
        header_text,
        Style::default()
            .fg(PICKER_ACCENT)
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(
        Paragraph::new(header_line),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let rule = Line::from(Span::styled(
        "─".repeat(w),
        Style::default().fg(PICKER_BORDER),
    ));
    f.render_widget(
        Paragraph::new(rule),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );

    let body_h = h.saturating_sub(2);
    if body_h == 0 {
        return;
    }
    let body_y = area.y + 2;

    // Anchor: files start at line 0; grep hits put the matched line ~1/3 down.
    let anchor = picker_preview_anchor_line(p);
    let anchor = anchor.min(cache.lines.len().saturating_sub(1));
    let view_top = if matches!(
        p.matches
            .get(p.selected)
            .map(|&(i, _)| &p.active_items()[i].value),
        Some(PickerValue::GrepHit { .. })
    ) {
        let third = body_h / 3;
        anchor.saturating_sub(third)
    } else {
        0
    };

    let line_num_w = (cache.lines.len()).to_string().len().max(3);
    let content_w = w.saturating_sub(line_num_w + 2);

    let mut body_lines: Vec<Line> = Vec::with_capacity(body_h);
    for row in 0..body_h {
        let line_idx = view_top + row;
        if line_idx >= cache.lines.len() {
            body_lines.push(Line::from(Span::styled(
                "~",
                Style::default().fg(PICKER_BORDER),
            )));
            continue;
        }
        let line_text = &cache.lines[line_idx];
        let chars: Vec<char> = line_text.chars().collect();
        let visible_chars: Vec<char> = chars.iter().take(content_w).copied().collect();

        // Decide if this row is the grep hit line — if so, give it a subtle
        // background so the user's eye snaps to it.
        let is_hit_line = !cache.placeholder
            && line_idx == anchor
            && matches!(
                p.matches
                    .get(p.selected)
                    .map(|&(i, _)| &p.active_items()[i].value),
                Some(PickerValue::GrepHit { .. })
            );

        let row_bg = if is_hit_line {
            Some(Color::DarkGray)
        } else {
            None
        };
        let mut row_spans: Vec<Span> = Vec::new();
        let num_text = format!(" {:>w$} ", line_idx + 1, w = line_num_w);
        let num_text_chars = count_chars(&num_text);
        let num_style = if is_hit_line {
            Style::default()
                .bg(Color::DarkGray)
                .fg(picker_pulse_accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(PICKER_BORDER)
        };
        row_spans.push(Span::styled(num_text, num_style));

        // Per-char styling: syntax base layer, plus optional row background.
        let line_byte_start = cache.line_byte_starts[line_idx];
        let line_text_byte_end = line_byte_start + line_text.len();

        let base_bg = row_bg;
        if cache.placeholder {
            row_spans.push(Span::styled(
                line_text.clone(),
                Style::default().fg(PICKER_DIM),
            ));
        } else {
            // Build per-char styles for visible chars only.
            let mut styles: Vec<Style> = vec![Style::default(); visible_chars.len()];
            for s in &cache.spans {
                if s.range.end <= line_byte_start || s.range.start >= line_text_byte_end {
                    continue;
                }
                let Some(style) = scope_style(s.scope) else {
                    continue;
                };
                let line_chars_start = char_index_in_byte_range(
                    line_text,
                    s.range.start.saturating_sub(line_byte_start),
                );
                let line_chars_end = char_index_in_byte_range(
                    line_text,
                    s.range.end.saturating_sub(line_byte_start),
                );
                for slot in styles
                    .iter_mut()
                    .take(line_chars_end.min(visible_chars.len()))
                    .skip(line_chars_start.min(visible_chars.len()))
                {
                    *slot = style;
                }
            }
            for (i, c) in visible_chars.iter().enumerate() {
                let mut style = styles[i];
                if let Some(bg) = base_bg {
                    style = style.bg(bg);
                }
                if is_hit_line {
                    style = style.add_modifier(Modifier::BOLD);
                }
                row_spans.push(Span::styled(c.to_string(), style));
            }
        }

        // Pad row out so the bg color stretches if a hit line.
        let used = num_text_chars + visible_chars.len();
        let pad = w.saturating_sub(used);
        if pad > 0 {
            let pad_style = if let Some(bg) = base_bg {
                Style::default().bg(bg)
            } else {
                Style::default()
            };
            row_spans.push(Span::styled(" ".repeat(pad), pad_style));
        }
        body_lines.push(Line::from(row_spans));
    }
    f.render_widget(
        Paragraph::new(body_lines),
        Rect::new(area.x, body_y, area.width, body_h as u16),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::render;
    use ratatui::backend::TestBackend;
    use vix_core::Buffer;
    use vix_picker::Utf32String;

    /// Build a Files picker over an in-memory item list, render it through a
    /// 100x30 TestBackend, and confirm we don't panic and that the list rect
    /// reaches the configured terminal width. Drives the new fullscreen
    /// renderer end-to-end (split layout + preview).
    #[test]
    fn fullscreen_picker_renders_without_panic() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let items: Vec<PickerItem> = ["src/main.rs", "src/lib.rs", "Cargo.toml"]
            .iter()
            .map(|p| PickerItem {
                display: p.to_string(),
                value: PickerValue::File((*p).into()),
                haystack: Utf32String::from(*p),
            })
            .collect();
        let picker = Picker::new(PickerKind::Files, items);
        ed.picker = Some(picker);

        let backend = TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();

        // The list rect should now point at a non-empty area inside the body.
        let rect = ed.last_picker_rect.expect("expected picker rect set");
        assert!(rect.width > 0);
        assert!(rect.height > 0);
        // Body starts at y=3 (tabs / prompt / separator).
        assert_eq!(rect.y, 3);
    }

    /// Narrow terminal: the renderer should fall back to the overlay layout
    /// instead of trying to fit a split pane in too few columns.
    #[test]
    fn narrow_terminal_falls_back_to_overlay() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let items: Vec<PickerItem> = ["a.rs", "b.rs"]
            .iter()
            .map(|p| PickerItem {
                display: p.to_string(),
                value: PickerValue::File((*p).into()),
                haystack: Utf32String::from(*p),
            })
            .collect();
        let picker = Picker::new(PickerKind::Files, items);
        ed.picker = Some(picker);

        let backend = TestBackend::new(40, 14);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();
        // Overlay layout sets the rect inside the centered floating box,
        // so width is bounded by 80% of the area, not the full terminal.
        let rect = ed.last_picker_rect.expect("expected picker rect set");
        assert!(rect.width as u32 <= (40 * 4 / 5));
    }
}
