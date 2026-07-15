//! Picker rendering: a single geometry-driven path that lays out both the
//! fullscreen split-pane pickers (Files/Grep/Buffers) and the centered
//! compact overlay (Symbols/CodeActions/Jumps), plus the narrow-terminal
//! degrade of a fullscreen kind down to the compact box.
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::picker::{
    fit_picker_row, substring_match_smart, wrap_picker_detail, Picker, PickerItem, PickerKind,
    PickerLayout, PickerValue,
};
use crate::theme::Theme;
use crate::util::{char_index_in_byte_range, count_chars, pad_or_trunc, take_end};
use crate::Editor;

/// One-line key hint for the picker footer/header, shared between the
/// compact and fullscreen layouts so the wording stays consistent. Gated
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

/// Resolved rectangles for one picker frame. `compute_geometry` fills these
/// once; the render body only reads them. A `None` slot means "not shown for
/// this layout": compact pickers have no `tabs_row`/`preview`, full pickers
/// no `detail`. `tabs_row.is_none()` doubles as the "this is the compact
/// box" discriminator (true compact kinds *and* a fullscreen kind degraded
/// on a narrow terminal both land here).
pub(crate) struct PickerGeometry {
    /// Whole picker area — the full terminal for fullscreen kinds, the
    /// centered box for compact ones. Backdrop is filled across this rect.
    pub outer: Rect,
    /// Row 0 strip for full layouts: Files/Grep tabs or the Buffers title.
    /// `None` for the compact box.
    pub tabs_row: Option<Rect>,
    /// The query/prompt row (full: "❯ query"; compact: the combined header).
    pub prompt: Rect,
    /// The scrolling result list. This is what `last_picker_rect` records.
    pub list: Rect,
    /// Right-hand preview pane (full kinds, width ≥ 80).
    pub preview: Option<Rect>,
    /// 2-row detail strip at the bottom of the compact box.
    pub detail: Option<Rect>,
    /// Footer hint row (full layouts). Zero-sized for compact, whose hints
    /// live inline in the header — guard on `footer.height > 0`.
    pub footer: Rect,
}

/// Lay out one picker frame for `kind` inside `area`. Fullscreen kinds get
/// the split layout when the terminal is large enough; on a narrow terminal
/// (< 50 cols or < 12 rows) they degrade to the same centered compact box
/// that Symbols/CodeActions/Jumps always use.
pub(crate) fn compute_geometry(kind: PickerKind, area: Rect) -> PickerGeometry {
    let full = matches!(kind.spec().layout, PickerLayout::Full);
    let narrow = area.width < 50 || area.height < 12;

    if full && !narrow {
        let w = area.width;
        let h = area.height;
        // Rows: 0 tabs · 1 prompt · 2 top-sep · 3..h-2 body · h-2 bot-sep · h-1 footer.
        let body_top = area.y + 3;
        let body_h = h.saturating_sub(5); // (h-2) bottom-sep row minus body_top(3)

        // Body split: ~38% list, rest preview, with one separator column.
        let split_enabled = w >= 80;
        let list_w = if split_enabled {
            ((w as u32 * 38 / 100)
                .max(28)
                .min((w as u32).saturating_sub(40))) as u16
        } else {
            w
        };
        let preview = if split_enabled {
            let preview_x = area.x + list_w + 1;
            let preview_w = w.saturating_sub(list_w + 1);
            Some(Rect::new(preview_x, body_top, preview_w, body_h))
        } else {
            None
        };

        PickerGeometry {
            outer: area,
            tabs_row: Some(Rect::new(area.x, area.y, w, 1)),
            prompt: Rect::new(area.x, area.y + 1, w, 1),
            list: Rect::new(area.x, body_top, list_w, body_h),
            preview,
            detail: None,
            footer: Rect::new(area.x, area.y + h - 1, w, 1),
        }
    } else {
        // Centered compact box: ~80% wide, 2/3 tall, clamped so it still
        // renders on small terminals.
        let w = ((area.width as u32 * 4 / 5).max(30).min(area.width as u32)) as u16;
        let h = ((area.height as u32 * 2 / 3).max(10).min(area.height as u32)) as u16;
        let x = area.x + (area.width - w) / 2;
        let y = area.y + (area.height - h) / 2;

        // Reserve a 2-row detail strip when the box is tall enough. Matches
        // the old overlay's `h >= 8` gate; when there's no selection the
        // strip renders blank (indistinguishable from empty list rows).
        let detail_rows: u16 = if h >= 8 { 2 } else { 0 };
        let list_h = h.saturating_sub(1 + detail_rows);
        let detail = if detail_rows > 0 {
            Some(Rect::new(x, y + 1 + list_h, w, detail_rows))
        } else {
            None
        };

        PickerGeometry {
            outer: Rect::new(x, y, w, h),
            tabs_row: None,
            prompt: Rect::new(x, y, w, 1),
            list: Rect::new(x, y + 1, w, list_h),
            preview: None,
            detail,
            footer: Rect::new(x, y, 0, 0),
        }
    }
}

pub(crate) fn render_picker(f: &mut ratatui::Frame, area: Rect, ed: &mut Editor) {
    let Some(kind) = ed.picker.as_ref().map(|p| p.kind.clone()) else {
        ed.last_picker_rect = None;
        ed.last_picker_list_rows = 0;
        return;
    };
    let geo = compute_geometry(kind.clone(), area);
    let is_compact = geo.tabs_row.is_none();

    // Preview I/O happens in the main loop (`refresh_preview`), not here — the
    // renderer only reads `p.previews`.

    let theme = ed.theme.clone();
    let list_rows = geo.list.height as usize;
    let selected = ed.picker.as_ref().map(|p| p.selected).unwrap_or(0);
    let scroll = if list_rows == 0 {
        0
    } else if selected >= list_rows {
        selected + 1 - list_rows
    } else {
        0
    };

    {
        let Some(p) = ed.picker.as_mut() else {
            ed.last_picker_rect = None;
            ed.last_picker_list_rows = 0;
            return;
        };
        p.last_list_rows = list_rows;

        // Backdrop: compact box gets the picker background; a fullscreen
        // picker owns the whole terminal and wipes to Reset.
        let ow = geo.outer.width as usize;
        let oh = geo.outer.height as usize;
        let blank: Vec<Line> = (0..oh).map(|_| Line::raw(" ".repeat(ow))).collect();
        if is_compact {
            f.render_widget(Paragraph::new(blank).style(theme.picker_bg), geo.outer);
        } else {
            f.render_widget(Paragraph::new(blank), geo.outer);
        }

        if is_compact {
            render_compact_body(f, &geo, p, &theme, scroll);
        } else {
            render_full_body(f, &geo, p, &theme, scroll);
        }
    }

    // Mouse hit-test bookkeeping. `last_picker_rect` is the *list* rect for
    // every kind now — the handler maps a clicked row straight to a list row
    // with no per-layout header offset.
    ed.last_picker_rect = Some(geo.list);
    ed.last_picker_scroll = scroll;
    ed.last_picker_list_rows = list_rows;
}

/// Compact centered-box body: single combined header row, a plain list with
/// an optional 2-char mark gutter, and a 2-row detail strip.
fn render_compact_body(
    f: &mut ratatui::Frame,
    geo: &PickerGeometry,
    p: &Picker,
    theme: &Theme,
    scroll: usize,
) {
    let bg = theme.picker_bg;
    let w = geo.outer.width as usize;
    let is_unified = p.kind.spec().supports_marks;
    let is_buffers = p.kind.spec().buffer_actions;

    // Header: " label > query   <hints>  <marks> <n/total> ".
    let kind_label = p.kind.spec().label;
    let prompt = format!(" {kind_label} > {}", p.query);
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
    let header_pad = w.saturating_sub(prompt.len() + count.len());
    let header = Line::from(vec![
        Span::styled(prompt, theme.picker_header),
        Span::styled(" ".repeat(header_pad), Style::default().bg(theme.border)),
        Span::styled(count, Style::default().bg(theme.border).fg(theme.dim)),
    ]);
    f.render_widget(Paragraph::new(header), geo.prompt);
    // Caret past the query in the header: " {label} > " is 1 + label + 3 cols.
    set_prompt_cursor(f, geo, 4 + count_chars(kind_label), &p.query);

    // Grep with a too-short query has no items; show a dim hint at the top
    // of the list instead of leaving it blank.
    let grep_hint = grep_short_query_hint(p);

    let list_rows = geo.list.height as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(list_rows);
    for row in 0..list_rows {
        let idx = scroll + row;
        match p.matches.get(idx) {
            Some(&(item_idx, _)) => {
                let item = &p.active_items()[item_idx];
                let is_sel = idx == p.selected;
                let style = if is_sel { theme.picker_selected } else { bg };
                let (gutter, content_w) = if is_unified {
                    let g = if p.marked.contains(&item_idx) {
                        "● "
                    } else {
                        "  "
                    };
                    (g, w.saturating_sub(2))
                } else {
                    ("", w)
                };
                let text = fit_picker_row(item, content_w);
                let row_text = format!("{gutter}{text}");
                let pad = w.saturating_sub(count_chars(&row_text));
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
                            Style::default().bg(bg.bg.unwrap()).fg(theme.dim),
                        )));
                        continue;
                    }
                }
                lines.push(Line::raw(""));
            }
        }
    }
    f.render_widget(Paragraph::new(lines), geo.list);

    // Detail strip: wrap the selected item's full display over the reserved
    // rows. Blank when nothing is selected.
    if let Some(detail_rect) = geo.detail {
        let detail_rows = detail_rect.height as usize;
        let detail_style = theme.picker_detail;
        let inner_w = w.saturating_sub(2);
        let detail = p
            .matches
            .get(p.selected)
            .map(|&(item_idx, _)| p.active_items()[item_idx].display.as_str())
            .unwrap_or("");
        let wrapped = wrap_picker_detail(detail, inner_w, detail_rows);
        let mut dlines: Vec<Line> = Vec::with_capacity(detail_rows);
        for row in 0..detail_rows {
            let text = wrapped.get(row).map(String::as_str).unwrap_or("");
            let content = format!("  {text}");
            let pad = w.saturating_sub(count_chars(&content));
            dlines.push(Line::from(vec![
                Span::styled(content, detail_style),
                Span::styled(" ".repeat(pad), detail_style),
            ]));
        }
        f.render_widget(Paragraph::new(dlines), detail_rect);
    }
}

/// Fullscreen split body: tabs/breadcrumb row, prompt, separators, list pane
/// with match-highlight + mark gutter, preview pane, and a footer hint row.
fn render_full_body(
    f: &mut ratatui::Frame,
    geo: &PickerGeometry,
    p: &mut Picker,
    theme: &Theme,
    scroll: usize,
) {
    let w = geo.outer.width as usize;
    let is_buffers = p.kind.spec().buffer_actions;
    let is_unified = p.kind.spec().supports_marks;
    let mode_files = matches!(p.kind, PickerKind::Files);

    // --- Row 0: tabs + breadcrumb + counts -----------------------------------
    let cwd_str = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let cwd_disp = if count_chars(&cwd_str) > 36 {
        format!("…{}", take_end(&cwd_str, 35))
    } else {
        cwd_str
    };
    let counts = format!(" {} / {} ", p.matches.len(), p.active_items().len());
    let bread = format!(" {}  ", cwd_disp);
    let active_style = Style::default()
        .fg(theme.accent_hi)
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(theme.dim);
    let tab_row = if is_buffers {
        let title = " [Buffers] ";
        let title_used = count_chars(title);
        let right_used = count_chars(&counts) + count_chars(&bread);
        let middle_pad = w.saturating_sub(title_used + right_used);
        Line::from(vec![
            Span::styled(title, active_style),
            Span::raw(" ".repeat(middle_pad)),
            Span::styled(bread, Style::default().fg(theme.dim)),
            Span::styled(counts, theme.picker_strong),
        ])
    } else {
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
            Span::styled(bread, Style::default().fg(theme.dim)),
            Span::styled(counts, theme.picker_strong),
        ])
    };
    if let Some(tabs) = geo.tabs_row {
        f.render_widget(Paragraph::new(tab_row), tabs);
    }

    // --- Row 1: prompt -------------------------------------------------------
    let arrow_style = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    // Prompt = " ❯ <query>". The caret is the terminal's own cursor (set
    // below) so it blinks natively without any redraw cadence of our own.
    let prompt_line = Line::from(vec![
        Span::raw(" "),
        Span::styled("❯ ", arrow_style),
        Span::styled(p.query.clone(), theme.picker_strong),
    ]);
    f.render_widget(Paragraph::new(prompt_line), geo.prompt);
    // Caret sits just past the query: 1 leading space + 2 cols of "❯ ".
    set_prompt_cursor(f, geo, 3, &p.query);

    // --- Separators (top just above list, bottom just below) -----------------
    let sep = Line::from(Span::styled(
        "─".repeat(w),
        Style::default().fg(theme.border),
    ));
    f.render_widget(
        Paragraph::new(sep.clone()),
        Rect::new(geo.outer.x, geo.list.y - 1, geo.outer.width, 1),
    );
    f.render_widget(
        Paragraph::new(sep),
        Rect::new(
            geo.outer.x,
            geo.list.y + geo.list.height,
            geo.outer.width,
            1,
        ),
    );

    // --- Body: list pane ------------------------------------------------------
    let body_h = geo.list.height as usize;
    let list_w = geo.list.width as usize;
    let selected = p.selected;
    // Layout per row: " ▶ ●  <text> " — reserve 4 cells for the selection
    // glyph, mark glyph, and separating spaces.
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

    let grep_hint = grep_short_query_hint(p);

    let mut list_lines: Vec<Line> = Vec::with_capacity(body_h);
    for row in 0..body_h {
        let match_idx = scroll + row;
        let Some(&(item_idx, _)) = p.matches.get(match_idx) else {
            if row == 0 {
                if let Some(hint) = &grep_hint {
                    list_lines.push(Line::from(Span::styled(
                        format!("   {hint}"),
                        Style::default().fg(theme.dim),
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
            theme.picker_selected
        } else {
            Style::default()
        };

        let sel_glyph = if is_sel { "▶" } else { " " };
        let mark_glyph = if is_marked { "●" } else { " " };
        let displayed = fit_picker_row(item, content_w);

        // fzf-style match highlight: bold the chars in the row that matched
        // the live query. Only safe when the row wasn't truncated. Grep rows
        // skip it (regex over file contents, not the path). Files uses a
        // substring run starting at the first query token's offset; fuzzy
        // kinds use the scorer's char set.
        let highlight_chars: std::collections::HashSet<usize> =
            if displayed == item.display && !matches!(p.kind, PickerKind::Grep) {
                if matches!(p.kind, PickerKind::Files) {
                    match p.query.split_whitespace().next() {
                        Some(tok) => match substring_match_smart(&item.display, tok) {
                            Some(byte_off) => {
                                let char_start = item.display[..byte_off].chars().count();
                                let char_count = tok.chars().count();
                                (char_start..char_start + char_count).collect()
                            }
                            None => std::collections::HashSet::new(),
                        },
                        None => std::collections::HashSet::new(),
                    }
                } else if p.query.is_empty() {
                    std::collections::HashSet::new()
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
                Style::default().fg(theme.accent_hi)
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
                            .bg(theme.picker_selected.bg.unwrap())
                            .fg(theme.match_hi)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(theme.match_hi)
                            .add_modifier(Modifier::BOLD)
                    }
                } else {
                    row_style
                };
                spans.push(Span::styled(c.to_string(), style));
            }
        }
        let used = row_chrome + count_chars(&displayed);
        let pad = list_w.saturating_sub(used);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), row_style));
        }
        list_lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(list_lines), geo.list);

    // --- Body: vertical separator + preview pane -----------------------------
    if let Some(preview_rect) = geo.preview {
        let sep_col_lines: Vec<Line> = (0..body_h)
            .map(|_| Line::from(Span::styled("│", Style::default().fg(theme.border))))
            .collect();
        f.render_widget(
            Paragraph::new(sep_col_lines),
            Rect::new(geo.list.x + geo.list.width, geo.list.y, 1, geo.list.height),
        );
        render_picker_preview_pane(f, preview_rect, p, theme);
    }

    // --- Footer hints --------------------------------------------------------
    if geo.footer.height > 0 {
        let footer_body = format!(" {}", picker_hint_line(&p.kind, is_unified, is_buffers));
        let mark_status = if !p.marked.is_empty() {
            format!(" [{} marked] ", p.marked.len())
        } else {
            String::new()
        };
        let footer_pad = w.saturating_sub(count_chars(&footer_body) + count_chars(&mark_status));
        let footer_line = Line::from(vec![
            Span::styled(footer_body, Style::default().fg(theme.dim)),
            Span::raw(" ".repeat(footer_pad)),
            Span::styled(
                mark_status,
                Style::default()
                    .fg(theme.match_hi)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(Paragraph::new(footer_line), geo.footer);
    }
}

/// Place the terminal's real cursor at the end of the query in the prompt
/// row, `prefix_cols` columns in from the prompt's left edge. ratatui hides
/// the cursor by default each frame, so setting it here makes the caret the
/// terminal's own — native blink, no redraw cadence. Clamped to the prompt.
fn set_prompt_cursor(
    f: &mut ratatui::Frame,
    geo: &PickerGeometry,
    prefix_cols: usize,
    query: &str,
) {
    let x = (geo.prompt.x as usize + prefix_cols + count_chars(query))
        .min(geo.prompt.x as usize + geo.prompt.width.saturating_sub(1) as usize)
        as u16;
    f.set_cursor_position((x, geo.prompt.y));
}

/// The dim "type N+ characters" hint shown in place of the list when a Grep
/// picker's query is below its minimum length. `None` for every other case.
fn grep_short_query_hint(p: &Picker) -> Option<String> {
    if matches!(p.kind, PickerKind::Grep) && p.query.chars().count() < p.kind.spec().min_query_len {
        Some(format!(
            "type {}+ characters to search",
            p.kind.spec().min_query_len
        ))
    } else {
        None
    }
}

// --- Preview pane ----------------------------------------------------------

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

/// Draw the preview pane for the highlighted Files/Grep row. Caller positions
/// the rect; we own every cell inside it.
pub(crate) fn render_picker_preview_pane(
    f: &mut ratatui::Frame,
    area: Rect,
    p: &Picker,
    theme: &Theme,
) {
    let w = area.width as usize;
    let h = area.height as usize;
    if w < 8 || h < 3 {
        return;
    }

    // Nothing highlighted (empty match list) → empty pane, even if the MRU
    // still holds a preview from a prior selection.
    if p.matches.get(p.selected).is_none() {
        return;
    }
    // Draw the most-recently-used preview. On a hit it's the current row's
    // preview (promoted to the front); mid-debounce it's the previous row's,
    // kept visible until the rebuild lands.
    let Some(cache) = p.previews.first() else {
        // Not built yet → empty pane (the background wipe already cleared us).
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
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(
        Paragraph::new(header_line),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let rule = Line::from(Span::styled(
        "─".repeat(w),
        Style::default().fg(theme.border),
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
                Style::default().fg(theme.border),
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
            Some(theme.border)
        } else {
            None
        };
        let mut row_spans: Vec<Span> = Vec::new();
        let num_text = format!(" {:>w$} ", line_idx + 1, w = line_num_w);
        let num_text_chars = count_chars(&num_text);
        let num_style = if is_hit_line {
            Style::default()
                .bg(theme.border)
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.border)
        };
        row_spans.push(Span::styled(num_text, num_style));

        // Per-char styling: syntax base layer, plus optional row background.
        let line_byte_start = cache.line_byte_starts[line_idx];
        let line_text_byte_end = line_byte_start + line_text.len();

        let base_bg = row_bg;
        if cache.placeholder {
            row_spans.push(Span::styled(
                line_text.clone(),
                Style::default().fg(theme.dim),
            ));
        } else {
            // Build per-char styles for visible chars only.
            let mut styles: Vec<Style> = vec![Style::default(); visible_chars.len()];
            for s in &cache.spans {
                if s.range.end <= line_byte_start || s.range.start >= line_text_byte_end {
                    continue;
                }
                let Some(style) = theme.scope_styles.get(s.scope).copied().flatten() else {
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
    /// reaches into the body. Drives the fullscreen split layout end-to-end.
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

    /// Narrow terminal: a fullscreen kind degrades to the centered compact
    /// box — no full-width list, no preview/tabs.
    #[test]
    fn narrow_terminal_degrades_to_compact() {
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
        // Compact geometry: centered ~80% box, so the list rect is narrower
        // than the terminal and offset from the left edge.
        let rect = ed.last_picker_rect.expect("expected picker rect set");
        assert!(rect.width as u32 <= (40 * 4 / 5));
        assert!(rect.x > 0, "compact box should be centered, not flush left");
    }

    /// Directly exercise the geometry: a wide fullscreen kind gets tabs +
    /// preview; the same kind on a narrow terminal gets neither.
    #[test]
    fn geometry_full_vs_narrow() {
        let wide = compute_geometry(PickerKind::Files, Rect::new(0, 0, 100, 30));
        assert!(wide.tabs_row.is_some(), "wide full picker has a tab strip");
        assert!(
            wide.preview.is_some(),
            "wide full picker has a preview pane"
        );
        assert!(wide.detail.is_none(), "full layout has no detail strip");
        assert_eq!(wide.list.y, 3);

        let narrow = compute_geometry(PickerKind::Files, Rect::new(0, 0, 40, 14));
        assert!(narrow.tabs_row.is_none(), "narrow degrades: no tab strip");
        assert!(narrow.preview.is_none(), "narrow degrades: no preview");
        assert_eq!(narrow.footer.height, 0, "compact box has no footer row");
    }
}
