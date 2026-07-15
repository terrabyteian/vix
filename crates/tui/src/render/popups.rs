use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::Editor;

pub(crate) fn render_hover(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    let Some(text) = ed.hover_popup.as_deref() else {
        return;
    };
    let max_w = (area.width as u32 * 2 / 3).max(30).min(area.width as u32) as u16;
    // Wrap text to max_w - 2 (1 col padding each side).
    let inner_w = max_w.saturating_sub(2).max(10) as usize;
    let mut wrapped: Vec<String> = Vec::new();
    for raw in text.lines() {
        if raw.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let mut remaining = raw;
        while !remaining.is_empty() {
            let take = remaining.chars().take(inner_w).collect::<String>();
            let n = take.chars().count();
            wrapped.push(take);
            remaining = &remaining[remaining
                .char_indices()
                .nth(n)
                .map(|(i, _)| i)
                .unwrap_or(remaining.len())..];
        }
    }
    let h = (wrapped.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(3);
    let w = max_w;
    let x = area.x + area.width.saturating_sub(w) - 1;
    let y = area.y + 1;
    let rect = Rect::new(x, y, w, h);

    let bg = ed.theme.hover_bg;
    let blank: Vec<Line> = (0..h).map(|_| Line::raw(" ".repeat(w as usize))).collect();
    f.render_widget(Paragraph::new(blank).style(bg), rect);

    let mut lines: Vec<Line> = Vec::with_capacity(h as usize);
    lines.push(Line::styled(
        " hover ".to_string() + &" ".repeat((w as usize).saturating_sub(7)),
        ed.theme.hover_header,
    ));
    for l in wrapped.iter().take((h as usize).saturating_sub(2)) {
        let pad = (w as usize).saturating_sub(l.chars().count() + 1);
        lines.push(Line::from(vec![Span::styled(
            format!(" {}{}", l, " ".repeat(pad)),
            bg,
        )]));
    }
    while lines.len() < h as usize {
        lines.push(Line::styled(" ".repeat(w as usize), bg));
    }
    f.render_widget(Paragraph::new(lines), rect);
}

/// Draw the completion popup anchored to the cursor. Opens below the cursor
/// if there's room, else above.
pub(crate) fn render_completion_popup(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    use vix_lsp::lsp_types::CompletionItemKind;
    let Some(popup) = ed.completion_popup.as_ref() else {
        return;
    };
    if popup.visible.is_empty() {
        return;
    }

    let (cursor_line, cursor_col) = ed.buffer.char_to_line_col(ed.sel.head);
    let total_lines = ed.buffer.len_lines();
    let gutter_width = total_lines.to_string().len().max(3) + 1;

    // Screen position of the cursor char (top-left of its cell).
    let screen_row = cursor_line.saturating_sub(ed.view_top);
    let screen_col = gutter_width + 2 + cursor_col;
    let anchor_x = area.x + screen_col as u16;
    let anchor_y = area.y + screen_row as u16;

    // Size the popup: up to 8 rows, width = longest label + kind badge.
    let max_rows = 8u16;
    let n = popup.visible.len() as u16;
    let rows = n.min(max_rows);
    let max_label = popup
        .visible
        .iter()
        .map(|&i| popup.items[i].label.chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 40);
    let width = (max_label as u16 + 4).min(area.width.saturating_sub(2));

    // Place below cursor if there's room; otherwise above.
    let below_room = area.height.saturating_sub(anchor_y - area.y + 1);
    let (y, h) = if below_room >= rows {
        (anchor_y + 1, rows)
    } else {
        let above_room = anchor_y.saturating_sub(area.y);
        let h = rows.min(above_room);
        (anchor_y.saturating_sub(h), h)
    };
    let x = anchor_x.min(area.x + area.width.saturating_sub(width));
    if h == 0 || width == 0 {
        return;
    }
    let rect = Rect::new(x, y, width, h);

    let bg = ed.theme.popup;
    let sel_bg = ed.theme.popup_selected;

    // Scroll the visible slice so `selected` is always in view.
    let start = popup
        .selected
        .saturating_sub(h as usize - 1)
        .min(popup.visible.len().saturating_sub(h as usize));
    let end = (start + h as usize).min(popup.visible.len());

    let mut lines: Vec<Line> = Vec::with_capacity(h as usize);
    for row in start..end {
        let item_idx = popup.visible[row];
        let item = &popup.items[item_idx];
        let kind_badge = match item.kind {
            Some(CompletionItemKind::FUNCTION) | Some(CompletionItemKind::METHOD) => "f",
            Some(CompletionItemKind::VARIABLE) | Some(CompletionItemKind::FIELD) => "v",
            Some(CompletionItemKind::CLASS) | Some(CompletionItemKind::STRUCT) => "t",
            Some(CompletionItemKind::ENUM) | Some(CompletionItemKind::ENUM_MEMBER) => "e",
            Some(CompletionItemKind::MODULE) => "m",
            Some(CompletionItemKind::KEYWORD) => "k",
            Some(CompletionItemKind::SNIPPET) => "s",
            _ => " ",
        };
        let label_w = (width as usize).saturating_sub(4);
        let label = item.label.chars().take(label_w).collect::<String>();
        let pad = (width as usize).saturating_sub(label.chars().count() + 4);
        let text = format!(" {} {}{} ", kind_badge, label, " ".repeat(pad));
        let style = if row == popup.selected { sel_bg } else { bg };
        lines.push(Line::from(Span::styled(text, style)));
    }
    f.render_widget(Paragraph::new(lines).style(bg), rect);
}
