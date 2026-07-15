use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use vix_core::Buffer;

use crate::editor::Editor;
use crate::render::render;

/// Idle poll timeout while wakerless channels (LSP events, streaming
/// scan/grep) are live — they only deliver when we poll, so keep ticks
/// short enough that diagnostics and streamed batches feel immediate.
const IDLE_POLL_LIVE_MS: u64 = 250;
/// Idle poll timeout with no live channels: nothing can change without an
/// input event, so ticks exist only as a cheap liveness heartbeat.
const IDLE_POLL_QUIET_MS: u64 = 1000;
/// Rollout safety net: force a repaint at least this often so a missed
/// dirty-flag path shows up as a 500 ms lag instead of a stuck screen.
/// Delete (set to u64::MAX) once dirty-flag coverage has been dogfooded.
const FORCED_REDRAW_MS: u64 = 500;

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
        let mut last_draw = Instant::now();
        while !ed.quit {
            // Channel work first: each of these marks the screen dirty when
            // it changes something visible.
            ed.drain_lsp_events();
            ed.flush_picker_query_if_due();
            ed.pump_picker_sources();
            // Preview I/O stays off the render path (cheap no-op unless a
            // fullscreen preview picker is open).
            crate::picker::preview::refresh_preview(&mut ed);
            ed.decay_yank_flash();
            if last_draw.elapsed() >= Duration::from_millis(FORCED_REDRAW_MS) {
                ed.request_redraw();
            }

            if ed.needs_redraw {
                // Pre-draw maintenance runs outside the draw closure; the
                // content pane is the terminal minus statusline + cmdline.
                let rows = term.size()?.height.saturating_sub(2) as usize;
                ed.update(rows);
                term.draw(|f| render(f, &mut ed))?;
                ed.needs_redraw = false;
                last_draw = Instant::now();
            }

            // Sleep until input, the next time-driven deadline (yank flash,
            // picker debounces), or an idle tick to service channels.
            let now = Instant::now();
            let idle = Duration::from_millis(if ed.has_live_channels() {
                IDLE_POLL_LIVE_MS
            } else {
                IDLE_POLL_QUIET_MS
            });
            let timeout = ed
                .next_wake_deadline()
                .map(|t| t.saturating_duration_since(now))
                .map_or(idle, |until_wake| until_wake.min(idle));
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        ed.handle_key(k);
                        ed.request_redraw();
                    }
                    Event::Mouse(m) => {
                        ed.handle_mouse(m);
                        ed.request_redraw();
                    }
                    Event::Resize(_, _) => {
                        // ratatui re-queries the size on draw; we just need
                        // to actually draw. (The old 10 Hz loop masked that
                        // this event was silently dropped.)
                        ed.request_redraw();
                    }
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
