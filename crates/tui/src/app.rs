use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::time::Duration;

use vix_core::Buffer;

use crate::editor::Editor;
use crate::render::render;

pub fn run(buffer: Buffer, open_files_picker: bool) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;

    let mut ed = Editor::new(buffer);
    // Editor::new defaults to the env-independent ANSI table (tests depend on
    // that); pick the real palette from the terminal environment here.
    ed.theme = crate::theme::Theme::detect();
    ed.ensure_lsp_open();
    if open_files_picker {
        ed.discard_active_on_swap = true;
        ed.open_files_picker();
    }
    let result = (|| -> io::Result<()> {
        while !ed.quit {
            ed.drain_lsp_events();
            ed.flush_picker_query_if_due();
            ed.pump_grep_results();
            // Keep the picker preview MRU current *before* drawing so preview
            // I/O stays off the render path (cheap no-op unless a fullscreen
            // preview picker is open).
            crate::picker::preview::refresh_preview(&mut ed);
            term.draw(|f| render(f, &mut ed))?;
            // Poll for input with a short timeout so LSP events get a chance
            // to flow in between keystrokes without blocking.
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => ed.handle_key(k),
                    Event::Mouse(m) => ed.handle_mouse(m),
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    // Always restore terminal even on error.
    terminal::disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        DisableMouseCapture,
        terminal::LeaveAlternateScreen
    )?;
    term.show_cursor()?;
    result
}
