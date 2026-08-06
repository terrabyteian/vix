//! Draw the rendered-markdown view: slice the pre-laid-out display lines
//! (built in `Editor::refresh_md_layout` during pre-draw maintenance) at
//! the current display-line scroll. No gutter, no cursor.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::Editor;

pub(crate) fn render_markdown(f: &mut ratatui::Frame, area: Rect, ed: &mut Editor) {
    // Stash geometry for mouse handling; no gutter in the rendered view.
    ed.last_content_rect = Some(area);
    ed.last_gutter_cols = 0;

    let rows = area.height as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    if let Some(layout) = &ed.md_layout {
        let start = ed.md_scroll.min(layout.lines.len());
        let end = (start + rows).min(layout.lines.len());
        lines.extend(layout.lines[start..end].iter().cloned());
    }
    // Tilde rows past the end of the document, like the raw view.
    while lines.len() < rows {
        lines.push(Line::styled("~", ed.theme.tilde));
    }
    f.render_widget(Paragraph::new(lines), area);

    // Mouse drag-selection highlight: cell-level restyle over the drawn
    // frame (no span surgery — merges bg over the existing fg styling).
    if let Some((a, b)) = ed.md_select {
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let first_vis = ed.md_scroll;
        let last_vis = ed.md_scroll + rows.saturating_sub(1);
        for line in start.0.max(first_vis)..=end.0.min(last_vis) {
            // First selected line starts at the anchor column, the last ends
            // at the head column (inclusive); middle lines span the pane.
            let c0 = if line == start.0 { start.1 } else { 0 };
            let c1 = if line == end.0 {
                end.1
            } else {
                area.width.saturating_sub(1) as usize
            };
            let c0 = (c0.min(c1) as u16).min(area.width.saturating_sub(1));
            let c1 = (c1 as u16).min(area.width.saturating_sub(1));
            let row = area.y + (line - ed.md_scroll) as u16;
            let seg = Rect {
                x: area.x + c0,
                y: row,
                width: c1 - c0 + 1,
                height: 1,
            };
            f.buffer_mut().set_style(seg, ed.theme.selection);
        }
    }
}
