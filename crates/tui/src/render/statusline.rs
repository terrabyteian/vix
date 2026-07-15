use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use vix_core::Mode;

use crate::lsp::diag_summary;
use crate::Editor;

pub(crate) fn render_statusline(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    let (line, col) = ed.buffer.char_to_line_col(ed.sel.head);
    let mode_style = match ed.mode {
        Mode::Normal => ed.theme.mode_normal,
        Mode::Insert => ed.theme.mode_insert,
        Mode::Visual | Mode::VisualLine => ed.theme.mode_visual,
        Mode::Command => ed.theme.mode_command,
    };
    let path = ed
        .buffer
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "[No Name]".into());
    let dirty = if ed.buffer.dirty() { " [+]" } else { "" };
    let buf_count = ed.other_buffers.len() + 1;
    let buf_info = if buf_count > 1 {
        // Position by stable buffer id so the counter advances as the user
        // cycles with `<Tab>` / `:bn` instead of being pinned at 1.
        let mut bids: Vec<u64> = ed.other_buffers.iter().map(|b| b.bid).collect();
        bids.push(ed.active_bid);
        bids.sort_unstable();
        let pos = bids
            .iter()
            .position(|b| *b == ed.active_bid)
            .map(|i| i + 1)
            .unwrap_or(1);
        format!("[{pos}/{buf_count}] ")
    } else {
        String::new()
    };
    let diag_info = diag_summary(ed);
    let right = format!(" {}{}{}:{} ", buf_info, diag_info, line + 1, col + 1);
    let left_mode = format!(" {} ", ed.mode.label());
    let middle_pad = (area.width as usize)
        .saturating_sub(left_mode.len() + path.len() + dirty.len() + right.len() + 1);
    let middle = format!(" {}{}{}", path, dirty, " ".repeat(middle_pad));
    let line_widget = Line::from(vec![
        Span::styled(left_mode, mode_style),
        Span::styled(middle, ed.theme.statusline),
        Span::styled(right, ed.theme.statusline),
    ]);
    f.render_widget(Paragraph::new(line_widget), area);
}

pub(crate) fn render_cmdline(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    let content = match ed.mode {
        Mode::Command => format!("{}{}", ed.cmdline_prompt, ed.cmdline),
        _ => ed.msg.clone(),
    };
    f.render_widget(Paragraph::new(content), area);
}
