//! Picker rendering: a single geometry-driven path that lays out three
//! layouts — the fullscreen split-pane picker (Buffers), the fullscreen
//! omnibox (Omni: full-width bordered input, blended result list with an
//! optional preview split, status footer), and the legacy centered compact overlay
//! (Symbols/CodeActions/Jumps, plus a narrow-terminal degrade of Buffers).
//! `compute_geometry` resolves rects for the active layout; the matching
//! `render_*_body` reads them.
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::path::Path;

use crate::picker::{
    fit_picker_row, layout_grep_row, omni_key_path, parse_omni_query, substring_match_smart,
    wrap_picker_detail, GrepRowLayout, ItemRef, Picker, PickerItem, PickerKind, PickerLayout,
    PickerValue, PreviewKey, SourceFilter, MIN_CONTENT_QUERY_LEN,
};
use crate::theme::Theme;
use crate::util::{char_index_in_byte_range, count_chars, pad_or_trunc, take_end};
use crate::Editor;

/// One-line key hint for the compact/full picker footers, kept consistent
/// across those layouts. Marks apply only to `supports_marks` kinds; buffer
/// management only to `Buffers`. (Omni carries its own footer.)
pub(crate) fn picker_hint_line(supports_marks: bool, buffer_actions: bool) -> String {
    let mut s = String::from("\u{2191}\u{2193}/C-j/C-k nav \u{00b7} <CR> open");
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
/// once; the render body only reads them. `layout_tag` says which layout was
/// chosen (it drives body dispatch and backdrop). A `None` slot means "not
/// shown for this layout": the compact box has no `tabs_row`/`preview`/
/// `input_block`, the full split no `detail`/`input_block`, the omnibox no
/// `tabs_row`/`detail`.
pub(crate) struct PickerGeometry {
    /// Which layout these rects were computed for.
    pub layout_tag: PickerLayout,
    /// Whole picker area — the full terminal for the full split and the
    /// omnibox, the centered box for compact. Backdrop is filled across this
    /// rect.
    pub outer: Rect,
    /// Row 0 strip for the full layout: the Buffers title. `None` otherwise.
    pub tabs_row: Option<Rect>,
    /// The bordered input box (omnibox layout, non-degraded). `None` for
    /// every other layout and for the borderless omni degrade.
    pub input_block: Option<Rect>,
    /// The query/prompt row (full: "❯ query"; omni: "> query"; compact: the
    /// combined header).
    pub prompt: Rect,
    /// The scrolling result list. This is what `last_picker_rect` records.
    pub list: Rect,
    /// Preview pane (full: right-hand at width ≥ 80; omni: full-width below
    /// the list at height ≥ 24).
    pub preview: Option<Rect>,
    /// 2-row detail strip at the bottom of the compact box.
    pub detail: Option<Rect>,
    /// Footer row (full + omni layouts). Zero-sized for compact, whose hints
    /// live inline in the header — guard on `footer.height > 0`.
    pub footer: Rect,
}

/// Lay out one picker frame for `kind` inside `area`. Dispatches on the
/// kind's layout: the omnibox always gets `omni_geometry` (which handles its
/// own narrow degrade); the full split gets `full_geometry`, degrading to the
/// centered compact box on a narrow terminal (< 50 cols or < 12 rows);
/// everything else is the compact box.
pub(crate) fn compute_geometry(kind: PickerKind, area: Rect) -> PickerGeometry {
    match kind.spec().layout {
        PickerLayout::Omni => omni_geometry(area),
        PickerLayout::Full if !(area.width < 50 || area.height < 12) => full_geometry(area),
        _ => compact_geometry(area),
    }
}

/// Fullscreen omnibox geometry (telescope style): a full-width bordered
/// input box flush at the top, the result list under it — splitting off a
/// full-width preview pane below it at height ≥ 24 — and a footer on the
/// bottom screen row. Degrades on a small terminal (< 46 cols or < 12 rows)
/// to a borderless full-width column anchored at the top.
fn omni_geometry(area: Rect) -> PickerGeometry {
    if area.width < 46 || area.height < 12 {
        // Borderless degrade: 1 input row, list fills the middle, 1 footer.
        let w = area.width;
        let list_h = area.height.saturating_sub(2);
        return PickerGeometry {
            layout_tag: PickerLayout::Omni,
            outer: area,
            tabs_row: None,
            input_block: None,
            prompt: Rect::new(area.x, area.y, w, 1),
            list: Rect::new(area.x, area.y + 1, w, list_h),
            preview: None,
            detail: None,
            footer: Rect::new(area.x, area.y + 1 + list_h, w, 1),
        };
    }

    let w = area.width;
    let h = area.height;
    // Rows: 0..3 bordered input (border, "> query", border) · 3..h-1 body ·
    // h-1 footer. Prompt is the input's inner row, inset one column past the
    // left border.
    let input_block = Rect::new(area.x, area.y, w, 3);
    let prompt = Rect::new(area.x + 1, area.y + 1, w.saturating_sub(2), 1);
    let body_top = area.y + 3;
    let body_h = h.saturating_sub(4);

    // Body split: list on top, preview below, 50/50.
    let split_enabled = h >= 24;
    let list_h = if split_enabled { body_h / 2 } else { body_h };
    let preview = if split_enabled {
        Some(Rect::new(area.x, body_top + list_h, w, body_h - list_h))
    } else {
        None
    };

    PickerGeometry {
        layout_tag: PickerLayout::Omni,
        outer: area,
        tabs_row: None,
        input_block: Some(input_block),
        prompt,
        list: Rect::new(area.x, body_top, w, list_h),
        preview,
        detail: None,
        footer: Rect::new(area.x, area.y + h - 1, w, 1),
    }
}

/// Fullscreen split geometry (Buffers): tabs row, prompt, list + preview
/// panes separated by a column, and a footer.
fn full_geometry(area: Rect) -> PickerGeometry {
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
        layout_tag: PickerLayout::Full,
        outer: area,
        tabs_row: Some(Rect::new(area.x, area.y, w, 1)),
        input_block: None,
        prompt: Rect::new(area.x, area.y + 1, w, 1),
        list: Rect::new(area.x, body_top, list_w, body_h),
        preview,
        detail: None,
        footer: Rect::new(area.x, area.y + h - 1, w, 1),
    }
}

/// Centered compact box geometry: ~80% wide, 2/3 tall, clamped so it still
/// renders on small terminals. Used by Symbols/CodeActions/Jumps and by a
/// narrow-terminal Buffers degrade.
fn compact_geometry(area: Rect) -> PickerGeometry {
    let w = ((area.width as u32 * 4 / 5).max(30).min(area.width as u32)) as u16;
    let h = ((area.height as u32 * 2 / 3).max(10).min(area.height as u32)) as u16;
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;

    // Reserve a 2-row detail strip when the box is tall enough. Matches the
    // old overlay's `h >= 8` gate; when there's no selection the strip
    // renders blank (indistinguishable from empty list rows).
    let detail_rows: u16 = if h >= 8 { 2 } else { 0 };
    let list_h = h.saturating_sub(1 + detail_rows);
    let detail = if detail_rows > 0 {
        Some(Rect::new(x, y + 1 + list_h, w, detail_rows))
    } else {
        None
    };

    PickerGeometry {
        layout_tag: PickerLayout::Compact,
        outer: Rect::new(x, y, w, h),
        tabs_row: None,
        input_block: None,
        prompt: Rect::new(x, y, w, 1),
        list: Rect::new(x, y + 1, w, list_h),
        preview: None,
        detail,
        footer: Rect::new(x, y, 0, 0),
    }
}

pub(crate) fn render_picker(f: &mut ratatui::Frame, area: Rect, ed: &mut Editor) {
    let Some(kind) = ed.picker.as_ref().map(|p| p.kind.clone()) else {
        ed.last_picker_rect = None;
        ed.last_picker_list_rows = 0;
        return;
    };
    let geo = compute_geometry(kind.clone(), area);
    // The full split owns the whole terminal and wipes to Reset; the omnibox
    // (also fullscreen now) and the compact box paint the picker background
    // across `geo.outer` instead.
    let overlay = !matches!(geo.layout_tag, PickerLayout::Full);

    // Preview I/O happens in the main loop (`refresh_preview`), not here — the
    // renderer only reads `p.previews`.

    let theme = ed.theme.clone();
    // Copied out before the `&mut ed.picker` borrow below. Row paths are
    // stored relative to this root (or absolute under it), so it's what the
    // breadcrumb, the preview header, and the preview cache key resolve
    // against.
    let root = ed.root.clone();
    // Drives the omni footer's Esc hint: "quit" when this is the launch
    // omnibox with nothing open yet, "close" otherwise.
    let quit_on_picker_close = ed.quit_on_picker_close;
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

        // Backdrop: the omnibox (whole screen) and compact box paint the
        // picker background across `outer`; the full split wipes to Reset.
        let ow = geo.outer.width as usize;
        let oh = geo.outer.height as usize;
        let blank: Vec<Line> = (0..oh).map(|_| Line::raw(" ".repeat(ow))).collect();
        if overlay {
            f.render_widget(Paragraph::new(blank).style(theme.picker_bg), geo.outer);
        } else {
            f.render_widget(Paragraph::new(blank), geo.outer);
        }

        match geo.layout_tag {
            PickerLayout::Omni => {
                render_omni_body(f, &geo, p, &theme, scroll, quit_on_picker_close, &root)
            }
            PickerLayout::Full => render_full_body(f, &geo, p, &theme, scroll, &root),
            PickerLayout::Compact => render_compact_body(f, &geo, p, &theme, scroll),
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
    let nav_hint = format!(" {} ", picker_hint_line(is_unified, is_buffers));
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
        p.candidate_count()
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

    let list_rows = geo.list.height as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(list_rows);
    for row in 0..list_rows {
        let idx = scroll + row;
        match p.matches.get(idx) {
            Some(&(item_ref, _)) => {
                let item = p.item(item_ref);
                let is_sel = idx == p.selected;
                let style = if is_sel { theme.picker_selected } else { bg };
                let (gutter, content_w) = if is_unified {
                    let g = if p.marked.contains(&item_ref) {
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
            None => lines.push(Line::raw("")),
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
            .map(|&(item_ref, _)| p.item(item_ref).display.as_str())
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
    root: &Path,
) {
    let w = geo.outer.width as usize;
    let is_buffers = p.kind.spec().buffer_actions;
    let is_unified = p.kind.spec().supports_marks;

    // --- Row 0: title + breadcrumb + counts ----------------------------------
    // Only Buffers renders through the fullscreen split now (Omni is compact);
    // the tab strip is a single title.
    let root_str = root.display().to_string();
    let root_disp = if count_chars(&root_str) > 36 {
        format!("…{}", take_end(&root_str, 35))
    } else {
        root_str
    };
    let counts = format!(" {} / {} ", p.matches.len(), p.candidate_count());
    let bread = format!(" {}  ", root_disp);
    let active_style = Style::default()
        .fg(theme.accent_hi)
        .add_modifier(Modifier::BOLD);
    let title = " [Buffers] ";
    let title_used = count_chars(title);
    let right_used = count_chars(&counts) + count_chars(&bread);
    let middle_pad = w.saturating_sub(title_used + right_used);
    let tab_row = Line::from(vec![
        Span::styled(title, active_style),
        Span::raw(" ".repeat(middle_pad)),
        Span::styled(bread, Style::default().fg(theme.dim)),
        Span::styled(counts, theme.picker_strong),
    ]);
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

    // Buffers is the only fullscreen kind, so its rows always live in
    // `p.items`. Borrow that field directly (not `p.item(..)`, which borrows
    // all of `p`) so `p.scorer` stays free to borrow mutably in the loop
    // below for the match-highlight pass.
    let active_items: &[PickerItem] = &p.items;

    let mut list_lines: Vec<Line> = Vec::with_capacity(body_h);
    for row in 0..body_h {
        let match_idx = scroll + row;
        let Some(&(item_ref, _)) = p.matches.get(match_idx) else {
            list_lines.push(Line::raw(""));
            continue;
        };
        let ItemRef::Item(item_idx) = item_ref else {
            // Fullscreen kinds only produce `Item` refs; anything else is a
            // bug, but render it blank rather than panic.
            list_lines.push(Line::raw(""));
            continue;
        };
        let item = &active_items[item_idx];
        let is_sel = match_idx == selected;
        let is_marked = p.marked.contains(&item_ref);
        let row_style = if is_sel {
            theme.picker_selected
        } else {
            Style::default()
        };

        let sel_glyph = if is_sel { "▶" } else { " " };
        let mark_glyph = if is_marked { "●" } else { " " };
        let displayed = fit_picker_row(item, content_w);

        // fzf-style match highlight: bold the chars in the row that matched
        // the live query, using the scorer's matched char set. Only safe when
        // the row wasn't truncated.
        let highlight_chars: std::collections::HashSet<usize> =
            if displayed == item.display && !p.query.is_empty() {
                p.scorer
                    .match_indices(&item.haystack, &p.query)
                    .into_iter()
                    .map(|i| i as usize)
                    .collect()
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
        let hi_style = row_style.fg(theme.match_hi).add_modifier(Modifier::BOLD);
        spans.extend(highlighted_spans(
            &displayed,
            &highlight_chars,
            row_style,
            hi_style,
        ));
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
        render_picker_preview_pane(f, preview_rect, p, theme, None, root);
    }

    // --- Footer hints --------------------------------------------------------
    if geo.footer.height > 0 {
        let footer_body = format!(" {}", picker_hint_line(is_unified, is_buffers));
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

// --- Omnibox body ----------------------------------------------------------

/// Render `text` as styled spans, applying `hi` to the char positions in
/// `hl_chars` and `base` to the rest, coalescing adjacent same-style runs so
/// a row with no highlight is a single span. Shared by every layout's list
/// (fullscreen nucleo highlight, omni file-name + snippet highlight).
fn highlighted_spans(
    text: &str,
    hl_chars: &std::collections::HashSet<usize>,
    base: Style,
    hi: Style,
) -> Vec<Span<'static>> {
    if hl_chars.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut spans: Vec<Span> = Vec::new();
    let mut cur = String::new();
    let mut cur_hi = false;
    for (i, c) in text.chars().enumerate() {
        let is_hi = hl_chars.contains(&i);
        if !cur.is_empty() && is_hi != cur_hi {
            let style = if cur_hi { hi } else { base };
            spans.push(Span::styled(std::mem::take(&mut cur), style));
        }
        cur.push(c);
        cur_hi = is_hi;
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, if cur_hi { hi } else { base }));
    }
    spans
}

/// Char indices of every query token's first occurrence in `displayed`
/// (smart-case substring). Used to bold matched runs of an omni file row.
fn file_highlight_chars(displayed: &str, query: &str) -> std::collections::HashSet<usize> {
    let mut set = std::collections::HashSet::new();
    for token in query.split_whitespace() {
        if let Some(b) = substring_match_smart(displayed, token) {
            let start = char_index_in_byte_range(displayed, b);
            let end = char_index_in_byte_range(displayed, b + token.len());
            set.extend(start..end);
        }
    }
    set
}

/// The omnibox mode indicator: normally the `All · Files · Content` filter
/// spans with the active label accented/bold and the others dimmed. In regex
/// mode (leading `/` sigil) the Tab-filter cycle is a no-op, so the tabs are
/// replaced by a `Regex` badge — plus `· invalid` in `diag_error` red when
/// the pattern failed to compile. Returns the spans and their column width.
fn filter_indicator_spans(p: &Picker, theme: &Theme) -> (Vec<Span<'static>>, usize) {
    let active = Style::default()
        .fg(theme.accent_hi)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.dim);
    let sep = Style::default().fg(theme.border);
    if p.regex_mode() {
        let mut spans: Vec<Span> = vec![Span::styled(" ", sep), Span::styled("Regex", active)];
        let mut width = 1 + count_chars("Regex");
        if p.grep_error.is_some() {
            spans.push(Span::styled(" \u{00b7} ", sep));
            spans.push(Span::styled("invalid", theme.diag_error));
            width += 3 + count_chars("invalid");
        }
        spans.push(Span::styled(" ", sep));
        width += 1;
        return (spans, width);
    }
    let labels = [
        ("All", SourceFilter::All),
        ("Files", SourceFilter::Files),
        ("Content", SourceFilter::Content),
    ];
    let mut spans: Vec<Span> = vec![Span::styled(" ", sep)];
    let mut width = 1usize;
    for (i, (label, f)) in labels.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" \u{00b7} ", sep));
            width += 3;
        }
        let style = if *f == p.filter { active } else { dim };
        spans.push(Span::styled(*label, style));
        width += count_chars(label);
    }
    spans.push(Span::styled(" ", sep));
    width += 1;
    (spans, width)
}

/// Fullscreen omnibox body: full-width bordered input box with an embedded
/// filter indicator, a blended result list (file-name + content rows) on
/// top with an optional full-width preview pane below, and a status footer
/// with counts / scanning / hint on the left and key hints on the right.
fn render_omni_body(
    f: &mut ratatui::Frame,
    geo: &PickerGeometry,
    p: &Picker,
    theme: &Theme,
    scroll: usize,
    quit_on_picker_close: bool,
    root: &Path,
) {
    let w = geo.outer.width as usize;
    let bg = theme.picker_bg;
    let border_style = Style::default().fg(theme.border);
    let arrow_style = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let (ind_spans, ind_w) = filter_indicator_spans(p, theme);

    // --- Input --------------------------------------------------------------
    if let Some(block) = geo.input_block {
        // Bordered box; the filter indicator rides the top border's right side
        // when it fits, otherwise a plain rule.
        let inner_w = block.width.saturating_sub(2) as usize;
        let mut top: Vec<Span> = vec![Span::styled("\u{250c}", border_style)];
        if ind_w + 2 <= inner_w {
            top.push(Span::styled(
                "\u{2500}".repeat(inner_w - ind_w),
                border_style,
            ));
            top.extend(ind_spans);
        } else {
            top.push(Span::styled("\u{2500}".repeat(inner_w), border_style));
        }
        top.push(Span::styled("\u{2510}", border_style));
        f.render_widget(
            Paragraph::new(Line::from(top)),
            Rect::new(block.x, block.y, block.width, 1),
        );

        // Middle row: left border, "> query", right border. The prompt rect
        // (inset one column) hosts the text + the terminal caret.
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", arrow_style),
                Span::styled(p.query.clone(), theme.picker_strong),
            ]))
            .style(bg),
            geo.prompt,
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("\u{2502}", border_style))).style(bg),
            Rect::new(block.x, geo.prompt.y, 1, 1),
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("\u{2502}", border_style))).style(bg),
            Rect::new(block.x + block.width - 1, geo.prompt.y, 1, 1),
        );

        let bottom = Line::from(vec![
            Span::styled("\u{2514}", border_style),
            Span::styled("\u{2500}".repeat(inner_w), border_style),
            Span::styled("\u{2518}", border_style),
        ]);
        f.render_widget(
            Paragraph::new(bottom),
            Rect::new(block.x, block.y + 2, block.width, 1),
        );
    } else {
        // Borderless degrade: "> query" with the indicator right-aligned when
        // it fits on the same row.
        let mut spans = vec![
            Span::styled("> ", arrow_style),
            Span::styled(p.query.clone(), theme.picker_strong),
        ];
        let used = 2 + count_chars(&p.query);
        if used + ind_w < w {
            spans.push(Span::styled(" ".repeat(w - used - ind_w), bg));
            spans.extend(ind_spans);
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(bg), geo.prompt);
    }
    set_prompt_cursor(f, geo, 2, &p.query);

    // --- Result list --------------------------------------------------------
    // Highlight lookups use the parsed pattern, sigil stripped. In literal
    // mode the pattern IS the raw query (parse returns it unchanged), so this
    // is correct for both modes. A regex pattern with metacharacters simply
    // misses the literal substring lookup → no highlight (already a handled
    // case); regex-aware highlighting is a follow-up.
    let hl_query = parse_omni_query(&p.query).pattern;
    let list_rows = geo.list.height as usize;
    let list_w = geo.list.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(list_rows);
    for row in 0..list_rows {
        let idx = scroll + row;
        let Some(&(item_ref, _)) = p.matches.get(idx) else {
            lines.push(Line::from(Span::styled(" ".repeat(list_w), bg)));
            continue;
        };
        let item = p.item(item_ref);
        let is_sel = idx == p.selected;
        let row_style = if is_sel { theme.picker_selected } else { bg };
        let hi_style = row_style.fg(theme.match_hi).add_modifier(Modifier::BOLD);

        // Mark gutter (2 cols): "● " for a marked ref, blank otherwise.
        let marked = p.marked.contains(&item_ref);
        let mut spans: Vec<Span> = vec![
            Span::styled(
                if marked { "\u{25cf}" } else { " " }.to_string(),
                if marked {
                    row_style.fg(theme.accent_hi)
                } else {
                    row_style
                },
            ),
            Span::styled(" ".to_string(), row_style),
        ];
        let content_w = list_w.saturating_sub(2);

        match &item.value {
            PickerValue::GrepHit { path, line } => {
                let rel = path.strip_prefix(root).unwrap_or(path);
                let GrepRowLayout {
                    prefix,
                    snippet,
                    hl,
                } = layout_grep_row(
                    &rel.to_string_lossy(),
                    *line,
                    &item.display,
                    hl_query,
                    content_w,
                );
                spans.push(Span::styled(prefix, row_style.fg(theme.dim)));
                spans.push(Span::styled(" ".to_string(), row_style));
                let hl_chars: std::collections::HashSet<usize> =
                    hl.map(|r| r.collect()).unwrap_or_default();
                spans.extend(highlighted_spans(&snippet, &hl_chars, row_style, hi_style));
            }
            _ => {
                let displayed = fit_picker_row(item, content_w);
                let hl_chars = if hl_query.trim().is_empty() {
                    std::collections::HashSet::new()
                } else {
                    file_highlight_chars(&displayed, hl_query)
                };
                spans.extend(highlighted_spans(
                    &displayed, &hl_chars, row_style, hi_style,
                ));
            }
        }

        // Pad the row out so the selected/background fill spans the full width.
        let used: usize = spans.iter().map(|s| count_chars(&s.content)).sum();
        if used < list_w {
            spans.push(Span::styled(" ".repeat(list_w - used), row_style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), geo.list);

    // --- Preview pane ---------------------------------------------------------
    // No separator row: the pane's own header (relative path) + thin rule at
    // the top of its rect is the visual divider between list and preview.
    if let Some(preview_rect) = geo.preview {
        // Center + mark the matched line when the selected row is a grep hit
        // AND its file's preview is the one actually in the pane. Mid-debounce
        // the pane may still show the PREVIOUS file's preview — the key check
        // makes it render un-centered rather than centering the wrong file at
        // the new line. File / recent rows always preview from the top.
        let focus_line = p
            .matches
            .get(p.selected)
            .map(|&(r, _)| p.item(r))
            .and_then(|item| match &item.value {
                PickerValue::GrepHit { path, line } => {
                    let key = PreviewKey::Path(omni_key_path(root, path));
                    p.previews
                        .first()
                        .filter(|c| c.key == key)
                        .map(|_| (*line as usize).saturating_sub(1))
                }
                _ => None,
            });
        render_picker_preview_pane(f, preview_rect, p, theme, focus_line, root);
    }

    // --- Footer -------------------------------------------------------------
    render_omni_footer(f, geo, p, theme, quit_on_picker_close);
}

/// The omnibox status footer: counts / scanning / short-query hint on the
/// left, key hints on the right, both dimmed on the picker background.
/// `quit_on_picker_close` mirrors `Editor::quit_on_picker_close`: true only
/// for the launch omnibox with nothing open yet, in which case Esc quits
/// vix rather than merely closing the overlay — the right-hand hint reflects
/// that.
fn render_omni_footer(
    f: &mut ratatui::Frame,
    geo: &PickerGeometry,
    p: &Picker,
    theme: &Theme,
    quit_on_picker_close: bool,
) {
    if geo.footer.height == 0 {
        return;
    }
    let w = geo.footer.width as usize;
    let dim = theme.picker_bg.fg(theme.dim);

    let files = p.file_match_count;
    let grep = p.grep_match_count;
    let empty_query = p.query.trim().is_empty();
    let regex = p.regex_mode();
    // Left side: an `[invalid regex]` marker (diag_error red) replaces the
    // counts when the regex pattern failed to compile; otherwise counts —
    // content-only in regex mode — plus scanning / short-query nudges.
    let mut left_spans: Vec<Span> = Vec::new();
    if regex && p.grep_error.is_some() {
        left_spans.push(Span::styled(" ".to_string(), dim));
        left_spans.push(Span::styled(
            "[invalid regex]".to_string(),
            theme.picker_bg.patch(theme.diag_error),
        ));
    } else {
        let mut left = if p.showing_recents() {
            // Empty query, All filter, recents view: count the recents shown
            // plus the honest total file count (hidden behind the view).
            format!("{} recent \u{00b7} {files} files", p.recent_items.len())
        } else if regex {
            // Regex mode is content-only: file counts are meaningless.
            format!("{grep} matches \u{00b7} regex")
        } else {
            match (empty_query, p.filter) {
                (true, _) | (false, SourceFilter::Files) => format!("{files} files"),
                (false, SourceFilter::Content) => format!("{grep} matches"),
                (false, SourceFilter::All) => format!("{files} files \u{00b7} {grep} matches"),
            }
        };
        // Still streaming? Regex mode only watches the content source (the
        // file scan feeds rows it will never show); otherwise the files
        // source, or the content source when it's shown. An invalid regex
        // never spins — nothing is running (handled by the branch above).
        let scanning = if regex {
            !p.grep_items_complete
        } else {
            !p.file_items_complete || (!p.grep_items_complete && p.content_visible())
        };
        if scanning {
            left.push_str(" \u{00b7} scanning\u{2026}");
        }
        // 1..MIN content (pattern) chars with the content source visible:
        // nudge for more. Measured after the `/` sigil, so `/a` still nudges.
        let qn = p.content_pattern_len();
        if p.content_visible() && (1..MIN_CONTENT_QUERY_LEN).contains(&qn) {
            left.push_str(&format!(
                " \u{00b7} type {MIN_CONTENT_QUERY_LEN}+ chars for content"
            ));
        }
        left_spans.push(Span::styled(format!(" {left}"), dim));
    }

    // Right side: Tab is a no-op in regex mode, so drop its hint there.
    let right = match (regex, quit_on_picker_close) {
        (true, true) => "Esc quit",
        (true, false) => "Esc close",
        (false, true) => "Tab filter \u{00b7} Esc quit",
        (false, false) => "Tab filter \u{00b7} Esc close",
    };
    let right = format!("{right} ");
    let left_w: usize = left_spans.iter().map(|s| count_chars(&s.content)).sum();
    let pad = w.saturating_sub(left_w + count_chars(&right));
    left_spans.push(Span::styled(" ".repeat(pad), dim));
    left_spans.push(Span::styled(right, dim));
    f.render_widget(Paragraph::new(Line::from(left_spans)), geo.footer);
}

// --- Preview pane ----------------------------------------------------------

/// Draw the preview pane for the highlighted row (Buffers rows and omni
/// file/grep rows). Caller positions the rect; we own every cell inside it.
/// With `focus_line: None` the pane renders from the top of the file. A
/// `Some(line)` (0-based) scrolls that line to the pane's vertical center
/// (clamped at file start/end) and marks its whole row with
/// `theme.selection` — the omni grep-hit case.
pub(crate) fn render_picker_preview_pane(
    f: &mut ratatui::Frame,
    area: Rect,
    p: &Picker,
    theme: &Theme,
    focus_line: Option<usize>,
    root: &Path,
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

    // Pane header: the root-relative path (falling back to the cache path as
    // stored — omni keys are already relative; `[No Name]` passes through),
    // with a thin bottom rule. The full path keeps same-basename files apart.
    let header_path = cache.path.strip_prefix(root).unwrap_or(&cache.path);
    let header_text = pad_or_trunc(&format!(" {} ", header_path.to_string_lossy()), w);
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

    // Scroll window: from the top by default; a focus line is centered,
    // clamped at the file's start (saturating_sub) and end (min max_top).
    let max_top = cache.lines.len().saturating_sub(body_h);
    let top = match focus_line {
        Some(fl) => fl.saturating_sub(body_h.saturating_sub(1) / 2).min(max_top),
        None => 0,
    };
    let line_num_w = (cache.lines.len()).to_string().len().max(3);
    let content_w = w.saturating_sub(line_num_w + 2);

    let mut body_lines: Vec<Line> = Vec::with_capacity(body_h);
    for row in 0..body_h {
        let line_idx = top + row;
        if line_idx >= cache.lines.len() {
            body_lines.push(Line::from(Span::styled(
                "~",
                Style::default().fg(theme.border),
            )));
            continue;
        }
        // The focus row carries `theme.selection` across its full width —
        // gutter, text, and trailing pad — patched over each span's own
        // style so syntax fg survives where the theme only sets a bg.
        let focused = focus_line == Some(line_idx);
        let mark = |s: Style| if focused { s.patch(theme.selection) } else { s };
        let line_text = &cache.lines[line_idx];
        let visible_chars: Vec<char> = line_text.chars().take(content_w).collect();

        let mut row_spans: Vec<Span> = Vec::new();
        let num_text = format!(" {:>w$} ", line_idx + 1, w = line_num_w);
        let num_text_chars = count_chars(&num_text);
        row_spans.push(Span::styled(
            num_text,
            mark(Style::default().fg(theme.border)),
        ));

        // Per-char styling: the syntax base layer.
        let line_byte_start = cache.line_byte_starts[line_idx];
        let line_text_byte_end = line_byte_start + line_text.len();

        if cache.placeholder {
            row_spans.push(Span::styled(
                line_text.clone(),
                mark(Style::default().fg(theme.dim)),
            ));
        } else {
            // Build per-char styles for visible chars only. `cache.spans` is
            // sorted by (and non-overlapping in) byte start, so it's also
            // sorted by byte end — a windowed walk from the first span that
            // could possibly reach this line replaces the old full-file scan.
            let mut styles: Vec<Style> = vec![Style::default(); visible_chars.len()];
            let first = cache
                .spans
                .partition_point(|s| s.range.end <= line_byte_start);
            for s in &cache.spans[first..] {
                if s.range.start >= line_text_byte_end {
                    break;
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
            // Run-group consecutive chars sharing a base style into one Span
            // instead of allocating a String + Span per character.
            let mut run = String::new();
            let mut run_style = Style::default();
            for (i, c) in visible_chars.iter().enumerate() {
                if i > 0 && styles[i] != run_style {
                    row_spans.push(Span::styled(std::mem::take(&mut run), mark(run_style)));
                }
                run_style = styles[i];
                run.push(*c);
            }
            if !run.is_empty() {
                row_spans.push(Span::styled(run, mark(run_style)));
            }
        }

        let used = num_text_chars + visible_chars.len();
        let pad = w.saturating_sub(used);
        if pad > 0 {
            row_spans.push(Span::styled(" ".repeat(pad), mark(Style::default())));
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

    /// Build a Buffers picker (the only fullscreen split kind) over an
    /// in-memory item list, render it through a 100x30 TestBackend, and
    /// confirm we don't panic and that the list rect reaches into the body.
    /// Drives the fullscreen split layout end-to-end.
    #[test]
    fn fullscreen_picker_renders_without_panic() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let items: Vec<PickerItem> = ["src/main.rs", "src/lib.rs", "Cargo.toml"]
            .iter()
            .enumerate()
            .map(|(i, p)| PickerItem {
                display: p.to_string(),
                value: PickerValue::BufferIndex(i),
                haystack: Utf32String::from(*p),
            })
            .collect();
        let picker = Picker::new(PickerKind::Buffers, items);
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

    /// Narrow terminal: a fullscreen kind (Buffers) degrades to the centered
    /// compact box — no full-width list, no preview/tabs.
    #[test]
    fn narrow_terminal_degrades_to_compact() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let items: Vec<PickerItem> = ["a.rs", "b.rs"]
            .iter()
            .enumerate()
            .map(|(i, p)| PickerItem {
                display: p.to_string(),
                value: PickerValue::BufferIndex(i),
                haystack: Utf32String::from(*p),
            })
            .collect();
        let picker = Picker::new(PickerKind::Buffers, items);
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

    /// Directly exercise the geometry: a wide fullscreen kind (Buffers) gets
    /// tabs + preview; the same kind on a narrow terminal degrades to the
    /// compact box. Buffers never lands on the Omni layout.
    #[test]
    fn geometry_full_vs_narrow() {
        let wide = compute_geometry(PickerKind::Buffers, Rect::new(0, 0, 100, 30));
        assert!(matches!(wide.layout_tag, PickerLayout::Full));
        assert!(wide.tabs_row.is_some(), "wide full picker has a tab strip");
        assert!(
            wide.preview.is_some(),
            "wide full picker has a preview pane"
        );
        assert!(wide.detail.is_none(), "full layout has no detail strip");
        assert_eq!(wide.list.y, 3);

        let narrow = compute_geometry(PickerKind::Buffers, Rect::new(0, 0, 40, 14));
        assert!(
            matches!(narrow.layout_tag, PickerLayout::Compact),
            "narrow Buffers degrades to the compact box, not Omni"
        );
        assert!(narrow.tabs_row.is_none(), "narrow degrades: no tab strip");
        assert!(narrow.preview.is_none(), "narrow degrades: no preview");
        assert_eq!(narrow.footer.height, 0, "compact box has no footer row");
    }

    /// Omni geometry: fullscreen — full-width bordered input flush at the
    /// top, list above a full-width preview pane (50/50 split at height
    /// ≥ 24), footer on the bottom screen row.
    #[test]
    fn omni_geometry_fullscreen_split() {
        let area = Rect::new(0, 0, 120, 40);
        let g = compute_geometry(PickerKind::Omni, area);
        assert!(matches!(g.layout_tag, PickerLayout::Omni));
        assert_eq!(g.outer, area, "omnibox owns the whole screen");
        // Bordered input: 3 rows, full width, flush top; prompt inset (1,1).
        let block = g.input_block.expect("bordered input block");
        assert_eq!(block, Rect::new(0, 0, 120, 3));
        assert_eq!((g.prompt.x, g.prompt.y), (1, 1));
        // Body rows 3..39 (36 rows): list on top at half the body height,
        // the full-width preview pane filling the rest below.
        assert_eq!(g.list, Rect::new(0, 3, 120, 18));
        let preview = g.preview.expect("preview pane at height >= 24");
        assert_eq!(preview, Rect::new(0, 21, 120, 18));
        // Footer sits on the bottom screen row, full width.
        assert_eq!(g.footer, Rect::new(0, 39, 120, 1));
        assert!(g.tabs_row.is_none() && g.detail.is_none());
    }

    /// Below the 24-row split gate the omnibox stays fullscreen but drops
    /// the preview pane: the list fills the body, bordered input still
    /// present. Width doesn't gate the split — a narrow-but-tall terminal
    /// gets the pane.
    #[test]
    fn omni_geometry_no_preview_when_short() {
        // Wide but short: no pane, list fills the body.
        let g = compute_geometry(PickerKind::Omni, Rect::new(0, 0, 100, 20));
        assert!(matches!(g.layout_tag, PickerLayout::Omni));
        assert!(g.input_block.is_some(), "bordered input above the degrade");
        assert!(g.preview.is_none(), "no preview below 24 rows");
        assert_eq!(g.list, Rect::new(0, 3, 100, 16));
        assert_eq!(g.footer, Rect::new(0, 19, 100, 1));

        // Narrow but tall: the split appears.
        let g = compute_geometry(PickerKind::Omni, Rect::new(0, 0, 46, 24));
        assert!(matches!(g.layout_tag, PickerLayout::Omni));
        assert_eq!(g.list, Rect::new(0, 3, 46, 10));
        assert_eq!(
            g.preview.expect("preview pane at height >= 24"),
            Rect::new(0, 13, 46, 10)
        );
    }

    /// Omni degrade (< 46 cols or < 12 rows): borderless, full-width, y = 0,
    /// with a 1-row input, a filling list, and a 1-row footer.
    #[test]
    fn omni_geometry_degrades_borderless() {
        let g = compute_geometry(PickerKind::Omni, Rect::new(0, 0, 40, 10));
        assert!(matches!(g.layout_tag, PickerLayout::Omni));
        assert!(g.input_block.is_none(), "degrade is borderless");
        assert_eq!(g.outer.width, 40, "degrade uses the full width");
        assert_eq!(g.prompt.y, 0, "input anchored at the top");
        assert_eq!(g.list.width, 40);
        assert_eq!(g.list.y, 1);
        assert_eq!(g.footer.y, g.list.y + g.list.height);
        assert_eq!(g.footer.height, 1);
    }

    /// Build an Omni picker with a file row and a content (grep) row, render
    /// it through a TestBackend, and confirm the visible cells carry the file
    /// name, the grep prefix + snippet, the footer counts, and the filter
    /// indicator — driving `render_omni_body` end-to-end.
    #[test]
    fn omni_body_renders_rows_footer_and_filter() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let mut p = Picker::new(PickerKind::Omni, Vec::new());
        p.file_items.push(PickerItem {
            display: "src/alpha.rs".to_string(),
            value: PickerValue::File("src/alpha.rs".into()),
            haystack: Utf32String::from("src/alpha.rs"),
        });
        p.grep_items.push(PickerItem {
            display: "let beta = 1;".to_string(),
            value: PickerValue::GrepHit {
                path: "src/other.rs".into(),
                line: 12,
            },
            haystack: Utf32String::from("src/other.rs:12: let beta = 1;"),
        });
        // Mid-scan so the "scanning…" indicator shows.
        p.file_items_complete = false;
        p.query = "alpha".to_string();
        p.rescore();
        ed.picker = Some(p);

        let backend = TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();
        let text = backend_text(term.backend());

        assert!(text.contains("alpha.rs"), "file row missing: {text}");
        assert!(
            text.contains("src/other.rs:12"),
            "grep prefix missing: {text}"
        );
        assert!(text.contains("let beta = 1;"), "snippet missing: {text}");
        assert!(text.contains("matches"), "footer counts missing: {text}");
        assert!(
            text.contains("scanning"),
            "scanning indicator missing: {text}"
        );
        assert!(
            text.contains("All") && text.contains("Files") && text.contains("Content"),
            "filter indicator missing: {text}"
        );
        assert!(text.contains("Tab filter"), "footer hints missing: {text}");
    }

    /// Regex mode (leading `/` sigil): the filter tabs give way to a `Regex`
    /// badge, the footer counts go content-only (`N matches · regex`), the
    /// Tab hint disappears (Tab is a no-op in regex mode), and the grep
    /// snippet still highlights via the stripped pattern.
    #[test]
    fn omni_regex_mode_shows_badge_and_content_footer() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let mut p = Picker::new(PickerKind::Omni, Vec::new());
        p.grep_items.push(PickerItem {
            display: "let beta = 1;".to_string(),
            value: PickerValue::GrepHit {
                path: "src/other.rs".into(),
                line: 12,
            },
            haystack: Utf32String::from("src/other.rs:12: let beta = 1;"),
        });
        p.query = "/beta".to_string();
        p.rescore();
        ed.picker = Some(p);

        let backend = TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();
        let text = backend_text(term.backend());

        assert!(text.contains("Regex"), "regex badge missing: {text}");
        assert!(
            !text.contains("All \u{00b7} Files \u{00b7} Content"),
            "filter tabs should be hidden in regex mode: {text}"
        );
        assert!(
            !text.contains("invalid"),
            "no invalid marker for a valid pattern: {text}"
        );
        assert!(
            text.contains("1 matches \u{00b7} regex"),
            "content-only regex counts missing: {text}"
        );
        assert!(
            !text.contains("Tab filter"),
            "Tab hint should be hidden in regex mode: {text}"
        );
        assert!(text.contains("Esc close"), "Esc hint missing: {text}");
        assert!(text.contains("let beta = 1;"), "snippet missing: {text}");
        // The prompt shows the query as typed, sigil included.
        assert!(text.contains("> /beta"), "raw prompt query missing: {text}");
    }

    /// An invalid regex pattern: the badge grows a `· invalid` marker, the
    /// footer swaps its counts for `[invalid regex]`, and the spinner stays
    /// off even mid-stream flags — nothing is running.
    #[test]
    fn omni_regex_invalid_shows_error_marker() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let mut p = Picker::new(PickerKind::Omni, Vec::new());
        p.query = "/[".to_string();
        p.grep_error = Some("invalid regex".to_string());
        p.grep_items_complete = false;
        p.file_items_complete = false;
        p.rescore();
        ed.picker = Some(p);

        let backend = TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();
        let text = backend_text(term.backend());

        assert!(
            text.contains("Regex \u{00b7} invalid"),
            "invalid badge missing: {text}"
        );
        assert!(
            text.contains("[invalid regex]"),
            "footer error marker missing: {text}"
        );
        assert!(
            !text.contains("scanning"),
            "no spinner while the regex is invalid: {text}"
        );
    }

    /// The short-query nudge measures the pattern AFTER the sigil: `/a` (1
    /// pattern char) shows the "type 2+ chars" hint, `/ab` does not.
    #[test]
    fn omni_regex_short_pattern_hint() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let mut p = Picker::new(PickerKind::Omni, Vec::new());
        p.query = "/a".to_string();
        p.rescore();
        ed.picker = Some(p);

        let backend = TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();
        let text = backend_text(term.backend());
        assert!(
            text.contains("type 2+ chars"),
            "short-pattern hint missing for /a: {text}"
        );

        let p = ed.picker.as_mut().unwrap();
        p.query = "/ab".to_string();
        p.rescore();
        let backend = TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();
        let text = backend_text(term.backend());
        assert!(
            !text.contains("type 2+ chars"),
            "no hint once the pattern reaches 2 chars: {text}"
        );
    }

    /// The launch omnibox (`quit_on_picker_close` true, as set by `app.rs`'s
    /// launch branch) owns the whole screen: no `[No Name]` statusline, no
    /// gutter/tilde filler bleeding through.
    #[test]
    fn launch_omnibox_renders_on_blank_screen() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        ed.quit_on_picker_close = true;
        ed.picker = Some(Picker::new(PickerKind::Omni, Vec::new()));

        let backend = TestBackend::new(80, 24);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();
        let text = backend_text(term.backend());

        assert!(
            !text.contains("NORMAL"),
            "launch screen should have no statusline mode text: {text}"
        );
        assert!(
            !text.contains('~'),
            "launch screen should have no tilde gutter filler: {text}"
        );

        // render_picker still records overlay geometry for mouse hit-testing.
        let rect = ed.last_picker_rect.expect("expected picker rect set");
        assert!(rect.width > 0);
        assert!(rect.height > 0);
    }

    /// Same picker, opened in-editor (`quit_on_picker_close` false) over a
    /// real buffer: the omnibox is fullscreen now, so the editor content,
    /// statusline, and cmdline are all skipped — no mode text, no tilde
    /// gutter peeking through.
    #[test]
    fn in_editor_omnibox_takes_full_screen() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        ed.quit_on_picker_close = false;
        ed.picker = Some(Picker::new(PickerKind::Omni, Vec::new()));

        let backend = TestBackend::new(80, 24);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();
        let text = backend_text(term.backend());

        assert!(
            !text.contains("NORMAL"),
            "fullscreen omnibox should hide the statusline: {text}"
        );
        assert!(
            !text.contains('~'),
            "fullscreen omnibox should hide the tilde gutter: {text}"
        );
    }

    /// Build a `PreviewCache` of `n` numbered lines ("content line 1"…)
    /// without touching the filesystem, keyed on `path` (relative, so it
    /// round-trips `omni_key_path` unchanged).
    fn numbered_cache(path: &str, n: usize) -> crate::picker::PreviewCache {
        let text: String = (1..=n).map(|i| format!("content line {i}\n")).collect();
        crate::picker::preview::build_preview_from_text(
            std::path::Path::new(path),
            &text,
            Vec::new(),
        )
    }

    /// Flatten the cells of rows `y0..` (the omni preview region at 40 tall:
    /// pane header at row 21, body below) to a newline-joined string.
    fn region_text(backend: &TestBackend, y0: u16) -> String {
        let buf = backend.buffer();
        let area = *buf.area();
        let mut out = String::new();
        for y in y0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// A selected File row with its preview cached: the pane below the list
    /// shows the file's content from the top. On a terminal too short for
    /// the split (< 24 rows) there is no pane and no content.
    #[test]
    fn omni_preview_pane_shows_file_content() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let mut p = Picker::new(PickerKind::Omni, Vec::new());
        p.file_items.push(PickerItem {
            display: "src/alpha.rs".to_string(),
            value: PickerValue::File("src/alpha.rs".into()),
            haystack: Utf32String::from("src/alpha.rs"),
        });
        p.query = "alpha".to_string();
        p.rescore();
        p.previews.push(numbered_cache("src/alpha.rs", 5));
        ed.picker = Some(p);

        let backend = TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();
        // Content shows in the pane (rows 21.. below the list), and only
        // there — the list rows above carry just the file-name row.
        let bottom = region_text(term.backend(), 21);
        assert!(
            bottom.contains("content line 1"),
            "preview content missing below the split: {bottom}"
        );
        let top = {
            let buf = term.backend().buffer();
            let mut s = String::new();
            for y in 0..21u16 {
                for x in 0..120u16 {
                    s.push_str(buf[(x, y)].symbol());
                }
                s.push('\n');
            }
            s
        };
        assert!(
            !top.contains("content line 1"),
            "preview content leaked into the list rows: {top}"
        );

        // 20 rows: omni layout without the split — no pane, no content.
        let backend = TestBackend::new(100, 20);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();
        let text = backend_text(term.backend());
        assert!(
            !text.contains("content line 1"),
            "no preview content without the split: {text}"
        );
    }

    /// A selected GrepHit row whose file's preview is in the pane: the
    /// matched line is scrolled to the pane's vertical center and its whole
    /// row carries `theme.selection`'s background.
    #[test]
    fn omni_grep_hit_centers_focus_line() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let mut p = Picker::new(PickerKind::Omni, Vec::new());
        p.grep_items.push(PickerItem {
            display: "content line 50".to_string(),
            value: PickerValue::GrepHit {
                path: "src/big.rs".into(),
                line: 50,
            },
            haystack: Utf32String::from("src/big.rs:50: content line 50"),
        });
        p.query = "content".to_string();
        p.rescore();
        p.previews.push(numbered_cache("src/big.rs", 100));
        ed.picker = Some(p);
        let sel_bg = ed.theme.selection.bg;

        let backend = TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();

        // Geometry (see `omni_geometry_fullscreen_split`): pane at y=21,
        // header y=21, rule y=22, body rows y=23..39 (16 rows). Centering
        // line 50 (idx 49) puts top at 42, so the focus row lands at
        // y = 23 + (49 - 42) = 30 — the pane's vertical middle.
        let bottom = region_text(term.backend(), 21);
        let buf = term.backend().buffer();
        // Header shows the root-relative path, not just the basename, so
        // same-named files in different directories stay distinguishable.
        let header_row: String = (0..120u16).map(|x| buf[(x, 21)].symbol()).collect();
        assert!(
            header_row.contains("src/big.rs"),
            "pane header should show the relative path: {header_row}"
        );
        assert!(
            bottom.contains("50 content line 50"),
            "focus line missing from the pane: {bottom}"
        );
        assert!(
            !bottom.contains("content line 1 "),
            "line 1 should be scrolled out when line 50 is centered: {bottom}"
        );
        let focus_row: String = (0..120u16).map(|x| buf[(x, 30)].symbol()).collect();
        assert!(
            focus_row.contains("50 content line 50"),
            "line 50 not at the pane's vertical middle: {focus_row}"
        );
        // The mark spans the full pane width: gutter, text, and trailing pad.
        for x in [0u16, 2, 40, 119] {
            assert_eq!(
                buf[(x, 30)].style().bg,
                sel_bg,
                "focus row cell at x={x} missing the selection bg"
            );
        }
        // A neighboring row is unmarked.
        assert_ne!(
            buf[(40, 29)].style().bg,
            sel_bg,
            "non-focus row should not carry the selection bg"
        );
    }

    /// Stale-guard: the selected GrepHit points at file B, but the pane still
    /// holds file A's preview (mid-debounce). The pane renders file A from
    /// the top — never centering the wrong file at B's line — with no
    /// selection-marked row.
    #[test]
    fn omni_stale_preview_not_centered() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let mut p = Picker::new(PickerKind::Omni, Vec::new());
        p.grep_items.push(PickerItem {
            display: "content line 50".to_string(),
            value: PickerValue::GrepHit {
                path: "src/b.rs".into(),
                line: 50,
            },
            haystack: Utf32String::from("src/b.rs:50: content line 50"),
        });
        p.query = "content".to_string();
        p.rescore();
        p.previews.push(numbered_cache("src/a.rs", 100));
        ed.picker = Some(p);
        let sel_bg = ed.theme.selection.bg;

        let backend = TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();

        let bottom = region_text(term.backend(), 21);
        assert!(
            bottom.contains("  1 content line 1"),
            "stale preview should render from the top: {bottom}"
        );
        let buf = term.backend().buffer();
        for y in 23..39u16 {
            for x in 0..120u16 {
                assert_ne!(
                    buf[(x, y)].style().bg,
                    sel_bg,
                    "stale preview must not mark any row (x={x}, y={y})"
                );
            }
        }
    }

    /// The empty-query recents view: a recent (File-valued) row previews
    /// from the top of the file — no centering.
    #[test]
    fn recents_row_previews_from_top() {
        let mut ed = Editor::new(Buffer::from_text("placeholder\n"));
        let mut p = Picker::new(PickerKind::Omni, Vec::new());
        p.recent_items.push(PickerItem {
            display: "src/rec.rs".to_string(),
            value: PickerValue::File("src/rec.rs".into()),
            haystack: Utf32String::from("src/rec.rs"),
        });
        p.rescore();
        assert!(p.showing_recents(), "empty query + recents = recents view");
        p.previews.push(numbered_cache("src/rec.rs", 100));
        ed.picker = Some(p);

        let backend = TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut ed)).unwrap();

        let bottom = region_text(term.backend(), 21);
        assert!(
            bottom.contains("  1 content line 1"),
            "recents preview should render from the top: {bottom}"
        );
        assert!(
            !bottom.contains("content line 50"),
            "recents preview should not be scrolled: {bottom}"
        );
    }

    /// Flatten a `TestBackend` buffer to a newline-joined string of its cell
    /// symbols, for substring assertions on rendered output.
    fn backend_text(backend: &TestBackend) -> String {
        let buf = backend.buffer();
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Build a `PreviewCache` of `n` uniform "lineNN\n" rows — each exactly 7
    /// bytes (a 4-char prefix, 2-digit zero-padded index, newline) — plus the
    /// given highlight spans, routed through `build_preview_from_text` so the
    /// build-time sort runs exactly as it does for real previews. Byte
    /// offsets are easy to hand-compute: line `i`'s text occupies
    /// `[7*i, 7*i + 6)`, and line `i+1` starts at `7*(i+1)`.
    fn windowed_cache(
        path: &str,
        n: usize,
        spans: Vec<vix_syntax::HlSpan>,
    ) -> crate::picker::PreviewCache {
        let text: String = (0..n).map(|i| format!("line{i:02}\n")).collect();
        crate::picker::preview::build_preview_from_text(std::path::Path::new(path), &text, spans)
    }

    /// The `"string"` scope's index into `HIGHLIGHT_NAMES` — a scope every
    /// theme in `theme.rs` actually maps to a style (see
    /// `ansi_scope_style_for`), so `theme.scope_styles.get(scope)` resolves
    /// to `Some` instead of silently no-op'ing.
    fn string_scope() -> usize {
        vix_syntax::HIGHLIGHT_NAMES
            .iter()
            .position(|n| *n == "string")
            .expect("theme scope table defines \"string\"")
    }

    /// Project root for the preview-pane tests. Their cache paths are
    /// already root-relative, so the pane's header strip is a no-op and any
    /// root that isn't a prefix of them will do.
    const TEST_ROOT: &str = "/projects/demo";

    /// A one-row Omni picker (a File row) with `cache` as its sole preview,
    /// ready to hand straight to `render_picker_preview_pane`.
    fn preview_picker(path: &str, cache: crate::picker::PreviewCache) -> Picker {
        let mut p = Picker::new(PickerKind::Omni, Vec::new());
        p.file_items.push(PickerItem {
            display: path.to_string(),
            value: PickerValue::File(path.into()),
            haystack: Utf32String::from(path),
        });
        p.matches = vec![(ItemRef::File(0), 0)];
        p.previews.push(cache);
        p
    }

    /// The renderer windows `cache.spans` per row via `partition_point`
    /// instead of scanning the whole file. Cover the four span placements
    /// relative to a scrolled preview window: fully before it, ending
    /// exactly at a visible line's `line_byte_start` (the partition_point
    /// boundary — must not bleed onto that line), spanning two lines, and
    /// fully past it. The spans are handed in deliberately out of byte
    /// order, so a correct result also proves `build_preview_from_text`'s
    /// defensive sort covers it.
    #[test]
    fn preview_pane_windows_spans_and_respects_sort_and_boundaries() {
        let scope = string_scope();
        let spans = vec![
            // Fully past the window (line 25) — listed first, out of order.
            vix_syntax::HlSpan {
                range: 7 * 25..7 * 25 + 6,
                scope,
            },
            // Boundary: all of line 12, ending exactly at line 13's
            // line_byte_start (84..91, and 7*13 == 91). Must style line 12,
            // must NOT style line 13.
            vix_syntax::HlSpan {
                range: 84..91,
                scope,
            },
            // Multiline: line 16 local offset 3 through line 17 local offset 3.
            vix_syntax::HlSpan {
                range: 115..122,
                scope,
            },
            // Fully before the scrolled-in window (line 5).
            vix_syntax::HlSpan {
                range: 7 * 5..7 * 5 + 6,
                scope,
            },
        ];
        let cache = windowed_cache("src/w.rs", 30, spans);
        let p = preview_picker("src/w.rs", cache);
        let theme = Theme::default_dark();

        // 20x10 pane: body_h = 8, centering line 15 (0-based) gives
        // top = 15 - (8-1)/2 = 12, so the visible window is lines 12..=19.
        // Body rows start at y=2, so line N renders at y = 2 + (N - 12).
        let backend = TestBackend::new(20, 10);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| {
            render_picker_preview_pane(
                f,
                Rect::new(0, 0, 20, 10),
                &p,
                &theme,
                Some(15),
                Path::new(TEST_ROOT),
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let styled = Some(ratatui::style::Color::LightYellow);
        // Unstyled cells: the syntax layer never touches them, so they carry
        // whatever the Paragraph widget's background fill left behind — not
        // `None`, `Color::Reset` (ratatui's default terminal-color marker).
        let unstyled = Some(ratatui::style::Color::Reset);

        // Line 12 (y=2): the boundary span covers it fully — chars 0..6,
        // i.e. x=5..11 (content starts after the 5-char gutter).
        for x in 5..11u16 {
            assert_eq!(
                buf[(x, 2)].style().fg,
                styled,
                "line12 x={x} should carry the boundary span"
            );
        }
        // Line 13 (y=3): the boundary span's end lands exactly on this
        // line's line_byte_start — must not bleed forward.
        for x in 5..11u16 {
            assert_eq!(
                buf[(x, 3)].style().fg,
                unstyled,
                "line13 x={x} must not be styled by the boundary span"
            );
        }
        // Line 16 (y=6): multiline span covers local chars[3..6) only.
        for x in 5..8u16 {
            assert_eq!(
                buf[(x, 6)].style().fg,
                unstyled,
                "line16 x={x} is before the multiline span's start"
            );
        }
        for x in 8..11u16 {
            assert_eq!(
                buf[(x, 6)].style().fg,
                styled,
                "line16 x={x} is inside the multiline span"
            );
        }
        // Line 17 (y=7): multiline span covers local chars[0..3) only.
        for x in 5..8u16 {
            assert_eq!(
                buf[(x, 7)].style().fg,
                styled,
                "line17 x={x} is inside the multiline span"
            );
        }
        for x in 8..11u16 {
            assert_eq!(
                buf[(x, 7)].style().fg,
                unstyled,
                "line17 x={x} is after the multiline span's end"
            );
        }
    }

    /// A focused row whose text carries a syntax span in the middle splits
    /// into three style runs (default prefix, scoped middle, default
    /// suffix) under the run-grouping refactor. The selection mark must
    /// still land on every run — not just the first — plus the gutter and
    /// trailing pad.
    #[test]
    fn preview_pane_focused_row_selection_spans_all_runs() {
        let scope = string_scope();
        // Line 1 ("line01"), local chars[2..4) = "ne" — byte range 9..11
        // (line 1 starts at byte 7).
        let spans = vec![vix_syntax::HlSpan {
            range: 9..11,
            scope,
        }];
        let cache = windowed_cache("src/f.rs", 3, spans);
        let p = preview_picker("src/f.rs", cache);
        let theme = Theme::default_dark();
        let sel = theme.selection;

        let backend = TestBackend::new(20, 10);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| {
            render_picker_preview_pane(
                f,
                Rect::new(0, 0, 20, 10),
                &p,
                &theme,
                Some(1),
                Path::new(TEST_ROOT),
            );
        })
        .unwrap();
        let buf = term.backend().buffer();

        // y=3: gutter (top=0, line1 is body row 1 -> y = 2 + 1). x=2 gutter,
        // x=5/x=10 default-style runs either side of the span, x=7/8 the
        // scoped run, x=19 the trailing pad.
        let y = 3;
        for x in [0u16, 2, 5, 7, 8, 10, 19] {
            assert_eq!(
                buf[(x, y)].style().bg,
                sel.bg,
                "x={x} missing the selection bg"
            );
            assert_eq!(
                buf[(x, y)].style().fg,
                sel.fg,
                "x={x} missing the selection fg"
            );
        }
        // A neighboring, non-focused row keeps its own (unmarked) style.
        assert_ne!(
            buf[(5, 2)].style().bg,
            sel.bg,
            "non-focus row should not carry the selection bg"
        );
    }
}
