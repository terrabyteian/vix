use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use vix_core::Mode;

use crate::lsp::diag_summary;
use crate::Editor;

pub(crate) fn render_statusline(f: &mut ratatui::Frame, area: Rect, ed: &Editor) {
    let (line, col) = ed.buffer.char_to_line_col(ed.sel.head);
    let mode_style = match ed.mode {
        Mode::Normal => Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        Mode::Insert => Style::default()
            .bg(Color::Green)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
        Mode::Visual | Mode::VisualLine => Style::default()
            .bg(Color::Magenta)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
        Mode::Command => Style::default()
            .bg(Color::Yellow)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
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
        Span::styled(
            middle,
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        Span::styled(right, Style::default().bg(Color::DarkGray).fg(Color::White)),
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
