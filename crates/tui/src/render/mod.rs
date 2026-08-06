use ratatui::layout::{Constraint, Direction, Layout};

use crate::picker::render::render_picker;
use crate::picker::PickerLayout;
use crate::Editor;

pub(crate) mod content;
pub(crate) mod markdown;
pub(crate) mod popups;
pub(crate) mod statusline;

use content::render_content;
use markdown::render_markdown;
use popups::{render_completion_popup, render_hover};
use statusline::{render_cmdline, render_statusline};

/// Draw one frame. State maintenance (cursor visibility, syntax cache, LSP
/// sync, yank-flash decay) happens in `Editor::update` / `decay_yank_flash`
/// on the main loop *before* the draw — this function only reads editor
/// state and stashes render geometry for mouse hit-testing.
pub(crate) fn render(f: &mut ratatui::Frame, ed: &mut Editor) {
    let area = f.area();

    // A fullscreen picker — the Buffers split or the omnibox — takes the
    // whole screen: skip drawing the editor, statusline, and cmdline so we
    // don't peek through.
    let fullscreen_picker = ed
        .picker
        .as_ref()
        .map(|p| {
            matches!(
                p.kind.spec().layout,
                PickerLayout::Full | PickerLayout::Omni
            )
        })
        .unwrap_or(false);
    if fullscreen_picker {
        render_picker(f, area, ed);
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

    if ed.view_mode == crate::ViewMode::Rendered {
        render_markdown(f, content_area, ed);
    } else {
        // Take the highlight cache out of `ed` so we can pass `&mut ed` and
        // the borrowed cache through render_content side by side. Restored
        // after.
        let hl_cache = std::mem::take(&mut ed.syntax_cache);
        render_content(f, content_area, ed, &hl_cache);
        ed.syntax_cache = hl_cache;
    }
    render_statusline(f, statusline_area, ed);
    render_cmdline(f, cmdline_area, ed);

    if ed.hover_popup.is_some() {
        render_hover(f, content_area, ed);
    }
    if ed.completion_popup.is_some() {
        render_completion_popup(f, content_area, ed);
    }
    if ed.picker.is_some() {
        // Only compact-layout pickers (Symbols/CodeActions/Jumps) reach this
        // in-editor overlay path; fullscreen layouts returned above.
        render_picker(f, content_area, ed);
    } else {
        ed.last_picker_rect = None;
        ed.last_picker_list_rows = 0;
    }
}
