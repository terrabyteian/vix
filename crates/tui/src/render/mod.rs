use ratatui::layout::{Constraint, Direction, Layout};
use std::time::Instant;

use crate::picker::is_fullscreen_picker_kind;
use crate::picker::render::{render_picker, render_picker_fullscreen};
use crate::Editor;

pub(crate) mod content;
pub(crate) mod popups;
pub(crate) mod statusline;

use content::render_content;
use popups::{render_completion_popup, render_hover};
use statusline::{render_cmdline, render_statusline};

pub(crate) fn render(f: &mut ratatui::Frame, ed: &mut Editor) {
    let area = f.area();

    // Files / Grep / Buffers picker takes the whole screen — skip drawing
    // the editor, statusline, and cmdline so we don't peek through.
    let fullscreen_picker = ed
        .picker
        .as_ref()
        .map(|p| is_fullscreen_picker_kind(&p.kind))
        .unwrap_or(false);
    if fullscreen_picker {
        // LSP sync still useful — keeps server state consistent across the
        // (potentially long) picker session.
        ed.sync_lsp_changes();
        render_picker_fullscreen(f, area, ed);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1), // statusline
            Constraint::Length(1), // cmdline / messages
        ])
        .split(area);

    let content_area = chunks[0];
    let statusline_area = chunks[1];
    let cmdline_area = chunks[2];

    ed.ensure_cursor_visible(content_area.height as usize);
    ed.refresh_syntax_cache();
    // Push any pending text changes to the LSP server before rendering. The
    // server's response (diagnostics) lands in the next event drain.
    ed.sync_lsp_changes();
    // Decay transient yank-flash overlay.
    if let Some((_, until)) = ed.yank_flash.as_ref() {
        if Instant::now() >= *until {
            ed.yank_flash = None;
        }
    }

    // Take the highlight cache out of `ed` so we can pass `&mut ed` and the
    // borrowed cache through render_content side by side. Restored after.
    let hl_cache = std::mem::take(&mut ed.syntax_cache);
    render_content(f, content_area, ed, &hl_cache);
    ed.syntax_cache = hl_cache;
    render_statusline(f, statusline_area, ed);
    render_cmdline(f, cmdline_area, ed);

    if ed.hover_popup.is_some() {
        render_hover(f, content_area, ed);
    }
    if ed.completion_popup.is_some() {
        render_completion_popup(f, content_area, ed);
    }
    if ed.picker.is_some() {
        render_picker(f, content_area, ed);
    } else {
        ed.last_picker_rect = None;
        ed.last_picker_list_rows = 0;
    }
}
