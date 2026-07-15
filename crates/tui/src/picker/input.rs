//! Picker input handling: key/mouse dispatch, picker lifecycle (open/toggle/
//! refresh), and the pick-commit path that acts on a selection.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vix_core::{Buffer, History, JumpEntry, Selection};
use vix_picker::{grep_cancellable, Utf32String};
use vix_syntax::{Language, Symbol, SyntaxState};

use crate::picker::{
    grep_as_picker_items, grep_hit_to_picker_item, scan_files_as_picker_items, Picker, PickerItem,
    PickerKind, PickerLayout, PickerValue, PICKER_REFRESH_DEBOUNCE_MS,
};
use crate::Editor;

/// What to do *after* picker-internal mutation completes. Lets
/// `handle_picker_key` release the `&mut Picker` borrow before calling
/// self-mutating `Editor` helpers.
pub(crate) enum PickerAction {
    None,
    Close,
    Select(PickerValue),
    SelectMany(Vec<PickerValue>),
    Toggle,
    // Re-score (Files) or re-grep (Grep) after the query changed.
    Refresh,
    // Buffer-picker management actions; idx is into the buffer list
    // (0 = active, 1.. = parked), not the picker match list.
    BufferSave(usize),
    BufferClose(usize, bool),
    BufferReload(usize, bool),
}

/// Buffer-management op requested by a Ctrl/Alt chord, resolved to the
/// highlighted row's buffer index by `buffer_op_action`.
enum BufOp {
    Save,
    Close(bool),
    Reload(bool),
}

/// Resolve a buffer-management chord against the highlighted row. Returns
/// `PickerAction::None` if the row isn't a `BufferIndex` (shouldn't happen
/// for the Buffers kind, but keeps this total).
fn buffer_op_action(p: &Picker, op: BufOp) -> PickerAction {
    let idx = p.matches.get(p.selected).and_then(|&(item_idx, _)| {
        if let PickerValue::BufferIndex(idx) = p.active_items()[item_idx].value {
            Some(idx)
        } else {
            None
        }
    });
    match (op, idx) {
        (BufOp::Save, Some(idx)) => PickerAction::BufferSave(idx),
        (BufOp::Close(force), Some(idx)) => PickerAction::BufferClose(idx, force),
        (BufOp::Reload(force), Some(idx)) => PickerAction::BufferReload(idx, force),
        _ => PickerAction::None,
    }
}

/// Enter: open the marked set (if any) or the highlighted row. Shared by
/// `picker_key_action`'s `KeyCode::Enter` arm.
fn enter_action(p: &Picker) -> PickerAction {
    if !p.marked.is_empty() {
        // Batch-open: collect marked items in match order so the
        // last-marked-in-list ends up active. Items not currently visible
        // (filtered out by the live query) are still picked up — we walk
        // all items, not just the match list.
        let mut values: Vec<PickerValue> = p
            .matches
            .iter()
            .filter(|(item_idx, _)| p.marked.contains(item_idx))
            .map(|(item_idx, _)| p.active_items()[*item_idx].value.clone())
            .collect();
        for &item_idx in &p.marked {
            if !p.matches.iter().any(|(i, _)| *i == item_idx) {
                values.push(p.active_items()[item_idx].value.clone());
            }
        }
        if values.is_empty() {
            PickerAction::Close
        } else {
            PickerAction::SelectMany(values)
        }
    } else if let Some(&(idx, _)) = p.matches.get(p.selected) {
        PickerAction::Select(p.active_items()[idx].value.clone())
    } else {
        PickerAction::Close
    }
}

/// PageUp/PageDown jump size: the list-row count from the picker's last
/// render, or a fixed fallback if it hasn't rendered yet (0).
fn picker_page_size(p: &Picker) -> usize {
    if p.last_list_rows == 0 {
        10
    } else {
        p.last_list_rows
    }
}

/// Ctrl-W: trim trailing whitespace, then delete back to the previous
/// whitespace boundary (or the start of the string).
fn delete_trailing_word(query: &mut String) {
    while query.ends_with(char::is_whitespace) {
        query.pop();
    }
    while let Some(c) = query.chars().next_back() {
        if c.is_whitespace() {
            break;
        }
        query.pop();
    }
}

/// Decide what a key event should do to the picker, mutating `p` in place
/// (selection, query, marks) and returning the follow-up action for the
/// caller to apply once the `&mut Picker` borrow is released.
///
/// The picker is always in typing mode (single-mode, fzf-style): any
/// printable key with no Ctrl/Alt modifier appends to `query`. Navigation,
/// marks, and picker-management actions all live on Ctrl/Alt chords or
/// non-printable keys so they never collide with query characters.
pub(crate) fn picker_key_action(p: &mut Picker, k: KeyEvent) -> PickerAction {
    let supports_marks = p.kind.spec().supports_marks;
    let buffer_actions = p.kind.spec().buffer_actions;
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);

    match k.code {
        KeyCode::Esc => return PickerAction::Close,
        KeyCode::Char('c') if ctrl => return PickerAction::Close,
        KeyCode::Tab => {
            return if supports_marks {
                PickerAction::Toggle
            } else {
                PickerAction::None
            };
        }
        KeyCode::Enter => return enter_action(p),
        KeyCode::Down => {
            p.move_selection(1);
            return PickerAction::None;
        }
        KeyCode::Up => {
            p.move_selection(-1);
            return PickerAction::None;
        }
        KeyCode::Home => {
            p.selected = 0;
            return PickerAction::None;
        }
        KeyCode::End => {
            p.selected = p.matches.len().saturating_sub(1);
            return PickerAction::None;
        }
        KeyCode::PageDown => {
            p.move_selection(picker_page_size(p) as isize);
            return PickerAction::None;
        }
        KeyCode::PageUp => {
            p.move_selection(-(picker_page_size(p) as isize));
            return PickerAction::None;
        }
        KeyCode::Backspace => {
            return if p.query.pop().is_some() {
                PickerAction::Refresh
            } else {
                PickerAction::None
            };
        }
        // Ctrl-Space: crossterm delivers this as `Char(' ')` + CONTROL (the
        // test DSL's `<Nul>` token produces the same event). Marks only
        // apply to kinds that support batch-open.
        KeyCode::Char(' ') if ctrl => {
            if supports_marks {
                if let Some(&(item_idx, _)) = p.matches.get(p.selected) {
                    if !p.marked.insert(item_idx) {
                        p.marked.remove(&item_idx);
                    }
                }
                p.move_selection(1);
            }
            return PickerAction::None;
        }
        KeyCode::Char('c') if alt => {
            if supports_marks {
                p.marked.clear();
            }
            return PickerAction::None;
        }
        KeyCode::Char('u') if ctrl => {
            p.query.clear();
            return PickerAction::Refresh;
        }
        KeyCode::Char('w') if ctrl => {
            delete_trailing_word(&mut p.query);
            return PickerAction::Refresh;
        }
        KeyCode::Char('n') | KeyCode::Char('j') if ctrl => {
            p.move_selection(1);
            return PickerAction::None;
        }
        KeyCode::Char('p') | KeyCode::Char('k') if ctrl => {
            p.move_selection(-1);
            return PickerAction::None;
        }
        KeyCode::Char('s') if ctrl && buffer_actions => {
            return buffer_op_action(p, BufOp::Save);
        }
        KeyCode::Char('q') if ctrl && buffer_actions => {
            return buffer_op_action(p, BufOp::Close(false));
        }
        KeyCode::Char('q') if alt && buffer_actions => {
            return buffer_op_action(p, BufOp::Close(true));
        }
        KeyCode::Char('r') if ctrl && buffer_actions => {
            return buffer_op_action(p, BufOp::Reload(false));
        }
        KeyCode::Char('r') if alt && buffer_actions => {
            return buffer_op_action(p, BufOp::Reload(true));
        }
        // Plain printable char (no Ctrl/Alt): types into the query. Shift
        // alone (e.g. capital letters) is fine here — only Ctrl/Alt divert
        // a char away from the query.
        KeyCode::Char(c) if !ctrl && !alt => {
            p.query.push(c);
            return PickerAction::Refresh;
        }
        _ => {}
    }
    PickerAction::None
}

/// Render label for the buffer picker: "[1] path   [+]".
pub(crate) fn label_for_buffer(buf: &Buffer, idx: usize, active: bool) -> String {
    let tag = if active { "%" } else { " " };
    let name = buf
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "[No Name]".into());
    let dirty = if buf.dirty() { " [+]" } else { "" };
    format!("[{:>2}] {}  {}{}", idx + 1, tag, name, dirty)
}

impl Editor {
    /// If the picker's query has been dirty for at least
    /// `PICKER_REFRESH_DEBOUNCE_MS`, fire the pending refresh. Files
    /// rescore on the main thread (cheap); Grep dispatches to a worker
    /// thread so the disk walk doesn't stall typing.
    pub(crate) fn flush_picker_query_if_due(&mut self) {
        let due = match self.picker.as_ref().and_then(|p| p.query_dirty_at) {
            Some(t) => {
                Instant::now().duration_since(t)
                    >= Duration::from_millis(PICKER_REFRESH_DEBOUNCE_MS)
            }
            None => return,
        };
        if !due {
            return;
        }
        if let Some(p) = self.picker.as_mut() {
            p.query_dirty_at = None;
        } else {
            return;
        }
        let is_grep = matches!(
            self.picker.as_ref().map(|p| &p.kind),
            Some(PickerKind::Grep)
        );
        if is_grep {
            self.start_grep_async();
        } else if let Some(p) = self.picker.as_mut() {
            p.rescore();
        }
    }

    /// Force-run the deferred rescore / regrep right now. Used on
    /// commit-style events (Enter, mouse click). Grep falls through to
    /// a synchronous walk so the matches list reflects the current query
    /// by the time the caller reads it; any in-flight async worker is
    /// cancelled via the generation atomic.
    pub(crate) fn flush_picker_query_now(&mut self) {
        let dirty = self
            .picker
            .as_ref()
            .is_some_and(|p| p.query_dirty_at.is_some());
        if let Some(p) = self.picker.as_mut() {
            p.query_dirty_at = None;
        } else {
            return;
        }
        let is_grep = matches!(
            self.picker.as_ref().map(|p| &p.kind),
            Some(PickerKind::Grep)
        );
        if is_grep {
            // Cancel any in-flight async worker by bumping the generation;
            // its result (if it ever arrives) won't match `grep_pending`'s
            // expected gen, and pump_grep_results discards it. Then run
            // sync so Enter / click can read p.matches immediately.
            if dirty || self.grep_pending.is_some() {
                self.grep_pending = None;
                self.grep_gen
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.refresh_grep_items();
            }
        } else if dirty {
            if let Some(p) = self.picker.as_mut() {
                p.rescore();
            }
        }
    }

    /// Spawn a worker thread that runs `grep_cancellable` for the picker's
    /// current query. Bumps the generation atomic so any older worker
    /// observes the change and quits its walk early. The worker sends
    /// results back over an mpsc channel; `pump_grep_results` drains it
    /// from the main loop.
    pub(crate) fn start_grep_async(&mut self) {
        let query = match self.picker.as_ref() {
            Some(p) if matches!(p.kind, PickerKind::Grep) => p.query.clone(),
            _ => return,
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let target_gen = self
            .grep_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .wrapping_add(1);
        let gen_arc = Arc::clone(&self.grep_gen);
        let (tx, rx) = std::sync::mpsc::channel::<Vec<PickerItem>>();
        std::thread::spawn(move || {
            let items: Vec<PickerItem> = if query.len() >= PickerKind::Grep.spec().min_query_len {
                grep_cancellable(&cwd, &query, &gen_arc, target_gen)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|g| grep_hit_to_picker_item(&cwd, g))
                    .collect()
            } else {
                Vec::new()
            };
            // Don't bother delivering if a newer generation has been issued
            // since — the receiver may already have been replaced.
            if gen_arc.load(std::sync::atomic::Ordering::SeqCst) == target_gen {
                let _ = tx.send(items);
            }
        });
        // Drop any prior receiver. Its worker (if still running) will see a
        // newer gen on its next cancel-check and quit.
        self.grep_pending = Some(rx);
    }

    /// Drain pending grep results, if any. Called once per main-loop tick.
    /// Applying happens only when the picker is still on Grep; otherwise we
    /// silently drop the result so a stale walk can't repopulate a closed
    /// or kind-toggled picker.
    pub(crate) fn pump_grep_results(&mut self) {
        let Some(rx) = self.grep_pending.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(items) => {
                self.grep_pending = None;
                if let Some(p) = self.picker.as_mut() {
                    if matches!(p.kind, PickerKind::Grep) {
                        p.grep_items = items;
                        p.marked.clear();
                        p.rescore();
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Worker quit (cancelled, or its sender went out of scope
                // without ever delivering). Nothing to do but clear.
                self.grep_pending = None;
            }
        }
    }

    /// One-word label for the active picker kind (test introspection).
    #[doc(hidden)]
    pub fn picker_selected_for_test(&self) -> usize {
        self.picker.as_ref().map(|p| p.selected).unwrap_or(0)
    }
    #[doc(hidden)]
    pub fn picker_marked_count_for_test(&self) -> usize {
        self.picker.as_ref().map(|p| p.marked.len()).unwrap_or(0)
    }
    #[doc(hidden)]
    pub fn picker_matches_count_for_test(&self) -> usize {
        self.picker.as_ref().map(|p| p.matches.len()).unwrap_or(0)
    }
    /// Force-run the deferred query refresh right now. Typing only marks the
    /// query dirty (debounced) so fast keystrokes on a large corpus don't
    /// pay per-char rescore cost; tests that need to observe `matches` /
    /// `selected` right after typing should call this first.
    #[doc(hidden)]
    pub fn flush_picker_query_for_test(&mut self) {
        self.flush_picker_query_now();
    }

    /// Open the file finder picker rooted at the current working directory.
    pub fn open_files_picker(&mut self) {
        self.open_picker_unified(PickerKind::Files, "");
    }

    /// Unified Files↔Grep picker. `<Tab>` toggles submode, query carries
    /// over. Files results are scanned once on open and cached so toggling
    /// is free.
    pub(crate) fn open_picker_unified(&mut self, initial: PickerKind, initial_query: &str) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let cached_files = scan_files_as_picker_items(&cwd);
        let items = match initial {
            PickerKind::Files => cached_files.clone(),
            PickerKind::Grep => {
                if initial_query.len() >= PickerKind::Grep.spec().min_query_len {
                    grep_as_picker_items(&cwd, initial_query)
                } else {
                    Vec::new()
                }
            }
            _ => return,
        };
        let mut p = Picker::new(initial, items).with_query(initial_query);
        // `file_items` doubles as the Files↔Grep toggle cache (see its doc
        // comment), so it's always populated here regardless of `initial` —
        // a picker opened straight into Grep still gets a free Files scan
        // banked for the first `<Tab>`.
        p.file_items = cached_files;
        self.picker = Some(p);
    }

    /// Toggle the active picker between Files and Grep submodes. The query
    /// is preserved. Files reuses `file_items` as-is (it persists across
    /// toggles — no rescan, no clone); Grep re-runs the live grep (if the
    /// query is at least `min_query_len` chars) to fill `grep_items`.
    pub(crate) fn toggle_picker_mode(&mut self) {
        let (new_kind, query) = {
            let Some(p) = self.picker.as_mut() else {
                return;
            };
            let new_kind = match p.kind {
                PickerKind::Files => PickerKind::Grep,
                PickerKind::Grep => PickerKind::Files,
                _ => return,
            };
            (new_kind, p.query.clone())
        };
        if matches!(new_kind, PickerKind::Grep) {
            let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
            let new_items = if query.len() >= PickerKind::Grep.spec().min_query_len {
                grep_as_picker_items(&cwd, &query)
            } else {
                Vec::new()
            };
            let Some(p) = self.picker.as_mut() else {
                return;
            };
            p.grep_items = new_items;
        }
        let Some(p) = self.picker.as_mut() else {
            return;
        };
        p.kind = new_kind;
        p.selected = 0;
        p.scroll = 0;
        // Item indices change when the active item storage is swapped, so
        // any previously-marked entries now point at the wrong rows.
        p.marked.clear();
        p.rescore();
    }

    /// Re-grep on each query change in Grep submode. Requires ≥2 chars;
    /// shorter queries clear the items list.
    pub(crate) fn refresh_grep_items(&mut self) {
        let query = match self.picker.as_ref() {
            Some(p) if matches!(p.kind, PickerKind::Grep) => p.query.clone(),
            _ => return,
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let new_items = if query.len() >= PickerKind::Grep.spec().min_query_len {
            grep_as_picker_items(&cwd, &query)
        } else {
            Vec::new()
        };
        let Some(p) = self.picker.as_mut() else {
            return;
        };
        p.grep_items = new_items;
        p.marked.clear();
        p.rescore();
    }

    /// Open the tree-sitter symbol picker for the current buffer.
    pub(crate) fn open_symbols_picker(&mut self) {
        let Some(s) = self.syntax.as_ref() else {
            self.msg = "no language bound".into();
            return;
        };
        let src = self.buffer.rope().to_string();
        let symbols: Vec<Symbol> = s.symbols(src.as_bytes()).unwrap_or_else(|e| {
            self.msg = format!("symbols: {e}");
            Vec::new()
        });
        if symbols.is_empty() && self.msg.is_empty() {
            self.msg = "no symbols found".into();
            return;
        }
        // Map byte offset → char offset (tree-sitter works in bytes; we jump
        // with char offsets).
        let rope = self.buffer.rope();
        let items: Vec<PickerItem> = symbols
            .into_iter()
            .map(|sym| {
                let char_off = rope.byte_to_char(sym.start_byte);
                let (line, _) = self.buffer.char_to_line_col(char_off);
                let display = format!("{:<8} {}  L{}", sym.kind, sym.name, line + 1);
                let haystack = Utf32String::from(sym.name.as_str());
                PickerItem {
                    display,
                    value: PickerValue::BufferOffset(char_off),
                    haystack,
                }
            })
            .collect();
        self.picker = Some(Picker::new(PickerKind::Symbols, items));
    }

    /// Open the grep picker, optionally pre-filled with `pattern`. Pattern
    /// becomes the initial query; the live grep machinery handles results.
    pub(crate) fn open_grep_picker(&mut self, pattern: &str) {
        self.open_picker_unified(PickerKind::Grep, pattern);
    }

    /// Handle a key event while the picker overlay is active. Returns true if
    /// the event was consumed; false means the picker closed itself.
    pub(crate) fn handle_picker_key(&mut self, k: KeyEvent) -> bool {
        // Enter commits a pick, which reads `p.matches` — flush any pending
        // query refresh first so the user lands on a result for the query
        // they just finished typing, not the one on screen mid-debounce.
        if matches!(k.code, KeyCode::Enter) {
            self.flush_picker_query_now();
        }

        let action = {
            let Some(p) = self.picker.as_mut() else {
                return false;
            };
            picker_key_action(p, k)
        };

        match action {
            PickerAction::None => {}
            PickerAction::Close => {
                self.picker = None;
            }
            PickerAction::Select(v) => {
                self.picker = None;
                self.pick_result(v);
            }
            PickerAction::SelectMany(values) => {
                self.picker = None;
                // Park every selection except the last directly into
                // `other_buffers` via `load_into_park` — that skips the
                // tree-sitter highlight construction and LSP `didOpen`
                // those buffers don't need until the user actually
                // activates them. The final item goes through
                // `pick_result`, which handles activation, jump-list
                // bookkeeping, and grep-hit cursor placement.
                if let Some((last, init)) = values.split_last() {
                    for v in init {
                        match v {
                            PickerValue::File(path) => {
                                self.load_into_park(path, None);
                            }
                            PickerValue::GrepHit { path, line } => {
                                self.load_into_park(path, Some(*line));
                            }
                            other => self.pick_result(other.clone()),
                        }
                    }
                    self.pick_result(last.clone());
                }
            }
            PickerAction::Toggle => {
                self.toggle_picker_mode();
            }
            PickerAction::Refresh => {
                // Mark the query dirty; the actual rescore / regrep is
                // deferred to `flush_picker_query_if_due` so rapid typing
                // doesn't pay the per-keystroke cost on large corpora.
                if let Some(p) = self.picker.as_mut() {
                    p.query_dirty_at = Some(Instant::now());
                }
            }
            PickerAction::BufferSave(idx) => {
                self.save_buffer_at_index(idx);
                self.refresh_buffers_picker_items();
            }
            PickerAction::BufferClose(idx, force) => {
                self.close_buffer_at_index(idx, force);
                if self.quit {
                    // Last buffer just closed → editor is exiting; tear down
                    // the picker so the final frame doesn't render over us.
                    self.picker = None;
                } else {
                    self.refresh_buffers_picker_items();
                }
            }
            PickerAction::BufferReload(idx, force) => {
                self.reload_buffer_at_index(idx, force);
                self.refresh_buffers_picker_items();
            }
        }
        true
    }

    /// Handle a mouse event while the picker overlay is active. Scroll
    /// moves the selection (so the visible window follows automatically);
    /// left-click on a row activates that entry. Clicks outside the overlay
    /// or on the header row are ignored.
    pub(crate) fn handle_picker_mouse(&mut self, me: MouseEvent) {
        // Left-click activates a row and reads `p.matches` to resolve it.
        // Same rationale as Enter: flush any deferred refresh so the click
        // lands on a result for the query the user is actually looking at.
        if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.flush_picker_query_now();
        }
        let selected_value: Option<PickerValue> = {
            let Some(p) = self.picker.as_mut() else {
                return;
            };
            match me.kind {
                MouseEventKind::ScrollUp => {
                    if p.selected > 0 {
                        p.selected -= 1;
                    }
                    None
                }
                MouseEventKind::ScrollDown => {
                    if p.selected + 1 < p.matches.len() {
                        p.selected += 1;
                    }
                    None
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    let Some(rect) = self.last_picker_rect else {
                        return;
                    };
                    if me.column < rect.x
                        || me.row < rect.y
                        || me.column >= rect.x + rect.width
                        || me.row >= rect.y + rect.height
                    {
                        return;
                    }
                    let row_in_rect = me.row - rect.y;
                    // For overlay pickers, row 0 of `rect` is the header; for
                    // the fullscreen picker, `rect` already starts at the
                    // first list row. Distinguish via picker kind.
                    let is_fullscreen = matches!(p.kind.spec().layout, PickerLayout::Full);
                    let list_row = if is_fullscreen {
                        row_in_rect as usize
                    } else if row_in_rect == 0 {
                        return;
                    } else {
                        (row_in_rect - 1) as usize
                    };
                    if list_row >= self.last_picker_list_rows {
                        return;
                    }
                    let match_idx = self.last_picker_scroll + list_row;
                    if let Some(&(item_idx, _)) = p.matches.get(match_idx) {
                        // First click on a row just focuses it; a second
                        // click on the same already-selected row activates.
                        if p.selected == match_idx {
                            Some(p.active_items()[item_idx].value.clone())
                        } else {
                            p.selected = match_idx;
                            None
                        }
                    } else {
                        return;
                    }
                }
                _ => return,
            }
        };
        if let Some(v) = selected_value {
            self.picker = None;
            self.pick_result(v);
        }
    }

    /// Act on a picker selection: open a file or jump to a grep hit.
    pub(crate) fn pick_result(&mut self, value: PickerValue) {
        match value {
            PickerValue::File(path) => self.open_path(&path),
            PickerValue::GrepHit { path, line } => {
                self.open_path(&path);
                // Jump to the hit line (1-based).
                let target = line.saturating_sub(1) as usize;
                let target = target.min(self.buffer.len_lines().saturating_sub(1));
                let ch = self.buffer.line_to_char(target);
                self.sel = Selection::at(ch).clamped(&self.buffer);
            }
            PickerValue::BufferOffset(ch) => {
                self.push_jump();
                self.sel = Selection::at(ch).clamped(&self.buffer);
            }
            PickerValue::BufferIndex(idx) => {
                if idx != 0 {
                    self.push_jump();
                }
                self.switch_to_buffer(idx);
            }
            PickerValue::CodeAction(idx) => {
                self.apply_code_action(idx);
            }
            PickerValue::JumpIndex(idx) => {
                self.apply_jump_pick(idx);
            }
        }
    }

    /// Save the buffer at `idx` (0 = active, 1.. = parked). Active buffer
    /// runs the LSP-format pass; parked buffers just write the rope to disk.
    pub(crate) fn save_buffer_at_index(&mut self, idx: usize) {
        if idx == 0 {
            self.format_and_save();
            return;
        }
        let Some(park) = self.other_buffers.get_mut(idx - 1) else {
            return;
        };
        if park.buffer.is_scratch() {
            self.msg = "E13: scratch buffer not writable".into();
            return;
        }
        match park.buffer.save() {
            Ok(()) => {
                self.msg = format!(
                    "\"{}\" written",
                    park.buffer
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                )
            }
            Err(e) => self.msg = format!("error: {e}"),
        }
    }

    /// Close the buffer at `idx`. For a parked buffer, refuses if dirty
    /// unless `force`. For the active buffer, delegates to `close_buffer`.
    pub(crate) fn close_buffer_at_index(&mut self, idx: usize, force: bool) {
        if idx == 0 {
            self.close_buffer(force);
            return;
        }
        let Some(park) = self.other_buffers.get(idx - 1) else {
            return;
        };
        if !force && park.buffer.dirty() {
            self.msg = "E89: No write since last change (use Q to force)".into();
            return;
        }
        // Detach LSP doc state for the closing buffer's path so a subsequent
        // re-open sends a fresh `didOpen`.
        let path = park.buffer.path().map(|p| p.to_path_buf());
        self.other_buffers.remove(idx - 1);
        if let Some(p) = path {
            if let Some(doc) = self.lsp_docs.remove(&p) {
                if let Some(client) = self.lsp_clients.get(&doc.server_cmd) {
                    client.did_close(doc.uri);
                }
            }
            self.diagnostics.remove(&p);
        }
    }

    /// Reload the buffer at `idx` from disk. Refuses if dirty unless `force`.
    pub(crate) fn reload_buffer_at_index(&mut self, idx: usize, force: bool) {
        if idx == 0 {
            self.reload_buffer(force);
            return;
        }
        let park_idx = idx - 1;
        let path = {
            let Some(park) = self.other_buffers.get(park_idx) else {
                return;
            };
            if park.buffer.is_scratch() {
                self.msg = "E382: scratch buffer cannot be reloaded".into();
                return;
            }
            if !force && park.buffer.dirty() {
                self.msg = "E37: No write since last change (use R to force)".into();
                return;
            }
            match park.buffer.path().map(|p| p.to_path_buf()) {
                Some(p) => p,
                None => {
                    self.msg = "E32: No file name".into();
                    return;
                }
            }
        };
        let new_buf = match Buffer::load(&path) {
            Ok(b) => b,
            Err(e) => {
                self.msg = format!("error: {e}");
                return;
            }
        };
        if let Some(park) = self.other_buffers.get_mut(park_idx) {
            let len = new_buf.len_chars();
            park.buffer = new_buf;
            park.history = History::new();
            park.pending_insert = None;
            park.last_change = None;
            park.sel = Selection::at(park.sel.head.min(len)).clamped(&park.buffer);
            park.view_top = 0;
            park.syntax =
                Language::from_path(path.as_path()).and_then(|l| SyntaxState::new(l).ok());
            park.syntax_cache.clear();
            park.syntax_version = None;
        }
        if let Some(doc) = self.lsp_docs.remove(&path) {
            if let Some(client) = self.lsp_clients.get(&doc.server_cmd) {
                client.did_close(doc.uri);
            }
        }
        self.diagnostics.remove(&path);
        self.msg = format!("\"{}\" reloaded", path.display());
    }

    /// `:jumps` — open a picker listing jump-list entries. Selection jumps to
    /// that entry. Informational column shows `>` next to the current position.
    pub(crate) fn open_jumps_picker(&mut self) {
        if self.jumps.is_empty() {
            self.msg = "jump list is empty".into();
            return;
        }
        let pos = self.jumps.pos();
        let entries: Vec<(usize, JumpEntry)> = self.jumps.entries().cloned().enumerate().collect();
        let mut items: Vec<PickerItem> = Vec::with_capacity(entries.len());
        for (i, e) in entries.iter() {
            let marker = if *i == pos { '>' } else { ' ' };
            let path = e
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "[No Name]".into());
            let label = format!(
                "{}  {:>3}  L{}:{}  {}",
                marker,
                i + 1,
                e.line + 1,
                e.col + 1,
                path
            );
            items.push(PickerItem {
                display: label.clone(),
                value: PickerValue::JumpIndex(*i),
                haystack: Utf32String::from(label.as_str()),
            });
        }
        self.picker = Some(Picker::new(PickerKind::Jumps, items));
    }

    /// Consume a `:jumps`-picker selection: jump to `entries[idx]` and update
    /// the internal `pos` so subsequent Ctrl-O/I walk from there. We walk the
    /// jump list by calling `back`/`forward` until pos matches — cheap enough
    /// for the 100-entry cap.
    pub(crate) fn apply_jump_pick(&mut self, idx: usize) {
        let cur = self.jumps.pos();
        if idx == cur {
            return;
        }
        if idx < cur {
            for _ in idx..cur {
                let c = self.current_jump_entry();
                if let Some(e) = self.jumps.back(c) {
                    if !self.goto_jump_entry(e) {
                        return;
                    }
                }
            }
        } else {
            for _ in cur..idx {
                if let Some(e) = self.jumps.forward() {
                    if !self.goto_jump_entry(e) {
                        return;
                    }
                }
            }
        }
    }

    /// Open the buffer picker: lists all live buffers for fuzzy selection.
    pub(crate) fn open_buffers_picker(&mut self) {
        let items = self.buffer_picker_items();
        self.picker = Some(Picker::new(PickerKind::Buffers, items));
    }

    /// Build the item list for the buffer picker from the current set of
    /// active + parked buffers. Used both on initial open and after a
    /// management action (save/reload/close) so labels reflect new state.
    pub(crate) fn buffer_picker_items(&self) -> Vec<PickerItem> {
        let mut items: Vec<PickerItem> = Vec::with_capacity(self.buffer_count());
        let active_label = label_for_buffer(&self.buffer, 0, true);
        items.push(PickerItem {
            display: active_label.clone(),
            value: PickerValue::BufferIndex(0),
            haystack: Utf32String::from(active_label.as_str()),
        });
        for (i, b) in self.other_buffers.iter().enumerate() {
            let label = label_for_buffer(&b.buffer, i + 1, false);
            items.push(PickerItem {
                display: label.clone(),
                value: PickerValue::BufferIndex(i + 1),
                haystack: Utf32String::from(label.as_str()),
            });
        }
        items
    }

    /// Rebuild the buffer picker's items in place. No-op if the picker isn't
    /// the Buffers kind. Drops the cached preview so the next render rebuilds
    /// it against any reloaded content.
    pub(crate) fn refresh_buffers_picker_items(&mut self) {
        if !matches!(
            self.picker.as_ref().map(|p| &p.kind),
            Some(PickerKind::Buffers)
        ) {
            return;
        }
        let items = self.buffer_picker_items();
        if let Some(p) = self.picker.as_mut() {
            p.items = items;
            p.preview = None;
            p.rescore();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn picker_click_below_list_rows_is_ignored() {
        let mut ed = Editor::new(Buffer::empty());
        let items: Vec<PickerItem> = ["alpha.rs", "beta.rs"]
            .into_iter()
            .map(|display| PickerItem {
                display: display.to_string(),
                value: PickerValue::File(display.into()),
                haystack: Utf32String::from(display),
            })
            .collect();
        let p = Picker::new(PickerKind::Files, items);
        ed.picker = Some(p);
        ed.last_picker_rect = Some(Rect::new(0, 0, 20, 5));
        ed.last_picker_scroll = 0;
        ed.last_picker_list_rows = 1;

        ed.handle_picker_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });

        let picker = ed.picker.as_ref().expect("picker should stay open");
        assert_eq!(picker.selected, 0);
    }
}
