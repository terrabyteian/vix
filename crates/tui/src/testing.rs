//! Test harness for driving the editor without a real terminal.
//!
//! `Harness` owns an `Editor` and drives it via `handle_key`. Key sequences
//! are written in a small vim-flavoured DSL: literal characters are sent as
//! `KeyCode::Char`, and angle-bracket tokens like `<Esc>`, `<CR>`, `<Tab>`,
//! `<C-x>` map to the obvious `KeyEvent`. Use `<lt>` for a literal `<`.

use crate::Editor;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::path::PathBuf;
use vix_core::{Buffer, JumpList, Mode, RepeatAction};

pub struct Harness {
    pub editor: Editor,
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    pub fn new() -> Self {
        Self {
            editor: Editor::new(Buffer::empty()),
        }
    }

    pub fn with_text(text: &str) -> Self {
        Self {
            editor: Editor::new(Buffer::from_text(text)),
        }
    }

    /// Build a harness with a buffer that already has a path attached. The
    /// path doesn't need to exist on disk — it's only used for syntax and
    /// LSP routing decisions, both of which are inert in tests.
    pub fn with_text_and_path(text: &str, path: impl Into<PathBuf>) -> Self {
        let mut buffer = Buffer::from_text(text);
        buffer.set_path(path.into());
        Self {
            editor: Editor::new(buffer),
        }
    }

    /// Send a vim-style key sequence: plain chars become `Char` events;
    /// angle-bracket tokens are decoded. Multi-token example: `i hi<Esc>0dw`.
    pub fn keys(&mut self, seq: &str) {
        for ev in parse_key_sequence(seq) {
            self.editor.handle_key(ev);
        }
    }

    /// Convenience: run an ex command. Equivalent to `keys(":<cmd><CR>")`
    /// but without having to escape `<` inside the command.
    pub fn cmd(&mut self, cmd: &str) {
        self.editor.handle_key(key(KeyCode::Char(':')));
        for c in cmd.chars() {
            self.editor.handle_key(key(KeyCode::Char(c)));
        }
        self.editor.handle_key(key(KeyCode::Enter));
    }

    pub fn text(&self) -> String {
        self.editor.buffer.rope().to_string()
    }

    /// Cursor as `(line, col)`, both 0-indexed.
    pub fn cursor(&self) -> (usize, usize) {
        self.editor.buffer.char_to_line_col(self.editor.sel.head)
    }

    pub fn mode(&self) -> Mode {
        self.editor.mode
    }

    pub fn msg(&self) -> &str {
        &self.editor.msg
    }

    pub fn cmdline(&self) -> &str {
        &self.editor.cmdline
    }

    pub fn dirty(&self) -> bool {
        self.editor.buffer.dirty()
    }

    pub fn buffer_count(&self) -> usize {
        self.editor.buffer_count()
    }

    pub fn parked_count(&self) -> usize {
        self.editor.parked_count()
    }

    pub fn register(&self) -> (&str, bool) {
        (self.editor.register_text(), self.editor.register_linewise())
    }

    pub fn jump_list(&self) -> &JumpList {
        self.editor.jump_list()
    }

    pub fn last_change(&self) -> Option<&RepeatAction> {
        self.editor.last_change()
    }

    pub fn picker_open(&self) -> bool {
        self.editor.picker_open()
    }

    pub fn picker_query(&self) -> Option<&str> {
        self.editor.picker_query()
    }

    pub fn picker_kind(&self) -> Option<&'static str> {
        self.editor.picker_kind_label()
    }

    pub fn quit_requested(&self) -> bool {
        self.editor.quit
    }

    pub fn active_language(&self) -> Option<vix_syntax::Language> {
        self.editor.active_language()
    }

    pub fn symbol_names(&self) -> Vec<String> {
        self.editor.symbol_names()
    }

    pub fn assert_text(&self, expected: &str) {
        assert_eq!(self.text(), expected, "buffer text mismatch");
    }

    /// Pretend a frame was rendered with the given content rect + gutter
    /// width. Required before `click(..)` / `scroll_*(..)` because the real
    /// values are normally populated by `render_content`.
    pub fn set_render_geometry(&mut self, content_rect: Rect, gutter_cols: u16) {
        self.editor
            .set_render_geometry_for_test(content_rect, gutter_cols);
    }

    /// Pretend the picker was just rendered with the given overlay rect and
    /// scroll offset. Required before calling `click(..)` against picker
    /// rows in tests, since `last_picker_rect` is normally populated by
    /// `render_picker`.
    pub fn set_picker_geometry(&mut self, rect: Rect, scroll: usize) {
        self.editor.set_picker_geometry_for_test(rect, scroll);
    }

    /// Simulate a left-button mouse-down at absolute terminal coords.
    pub fn click(&mut self, col: u16, row: u16) {
        self.editor.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }

    /// Simulate a left-button drag at absolute terminal coords.
    pub fn drag(&mut self, col: u16, row: u16) {
        self.editor.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }

    pub fn scroll_up(&mut self) {
        self.editor.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
    }

    pub fn scroll_down(&mut self) {
        self.editor.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
    }

    pub fn view_top(&self) -> usize {
        self.editor.view_top
    }

    pub fn assert_cursor(&self, line: usize, col: usize) {
        assert_eq!(
            self.cursor(),
            (line, col),
            "cursor at {:?}, expected ({line}, {col})",
            self.cursor()
        );
    }

    pub fn assert_mode(&self, mode: Mode) {
        assert_eq!(self.mode(), mode, "mode mismatch");
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

/// Parse a vim-style key sequence into a stream of `KeyEvent`s.
fn parse_key_sequence(seq: &str) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let bytes = seq.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(end) = seq[i + 1..].find('>') {
                let tok = &seq[i + 1..i + 1 + end];
                out.push(decode_token(tok));
                i += end + 2;
                continue;
            }
        }
        // Default: take the next char as a literal Char event.
        let ch = seq[i..].chars().next().unwrap();
        out.push(key(KeyCode::Char(ch)));
        i += ch.len_utf8();
    }
    out
}

/// Decode a token like `Esc`, `CR`, `C-x`, `S-Tab`. Case-insensitive.
fn decode_token(tok: &str) -> KeyEvent {
    let lower = tok.to_ascii_lowercase();
    match lower.as_str() {
        "esc" => return key(KeyCode::Esc),
        "cr" | "enter" | "return" => return key(KeyCode::Enter),
        "bs" | "backspace" => return key(KeyCode::Backspace),
        "tab" => return key(KeyCode::Tab),
        "space" => return key(KeyCode::Char(' ')),
        "up" => return key(KeyCode::Up),
        "down" => return key(KeyCode::Down),
        "left" => return key(KeyCode::Left),
        "right" => return key(KeyCode::Right),
        "pageup" | "pgup" => return key(KeyCode::PageUp),
        "pagedown" | "pgdn" => return key(KeyCode::PageDown),
        "lt" => return key(KeyCode::Char('<')),
        "gt" => return key(KeyCode::Char('>')),
        "nul" => return key_mod(KeyCode::Char(' '), KeyModifiers::CONTROL),
        _ => {}
    }
    // Modified forms: C-x, S-x, C-S-x.
    let mut mods = KeyModifiers::NONE;
    let mut rest = lower.as_str();
    loop {
        if let Some(r) = rest.strip_prefix("c-") {
            mods |= KeyModifiers::CONTROL;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("s-") {
            mods |= KeyModifiers::SHIFT;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("a-") {
            mods |= KeyModifiers::ALT;
            rest = r;
        } else {
            break;
        }
    }
    if rest.len() == 1 {
        let c = rest.chars().next().unwrap();
        return key_mod(KeyCode::Char(c), mods);
    }
    // Modified named key, e.g. <S-Tab>.
    let code = match rest {
        "esc" => KeyCode::Esc,
        "cr" | "enter" | "return" => KeyCode::Enter,
        "bs" | "backspace" => KeyCode::Backspace,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        other => panic!("unknown key token: <{other}>"),
    };
    key_mod(code, mods)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_chars() {
        let evs = parse_key_sequence("abc");
        assert_eq!(evs.len(), 3);
        assert!(matches!(evs[0].code, KeyCode::Char('a')));
        assert!(matches!(evs[2].code, KeyCode::Char('c')));
    }

    #[test]
    fn parses_named_tokens() {
        let evs = parse_key_sequence("i hi<Esc>");
        assert_eq!(evs.len(), 5);
        assert!(matches!(evs[0].code, KeyCode::Char('i')));
        assert!(matches!(evs[1].code, KeyCode::Char(' ')));
        assert!(matches!(evs[4].code, KeyCode::Esc));
    }

    #[test]
    fn parses_ctrl_tokens() {
        let evs = parse_key_sequence("<C-r>");
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0].code, KeyCode::Char('r')));
        assert!(evs[0].modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn lt_escape() {
        let evs = parse_key_sequence("<lt>");
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0].code, KeyCode::Char('<')));
    }

    #[test]
    fn harness_smoke_insert_text() {
        let mut h = Harness::new();
        h.keys("ihello world<Esc>");
        h.assert_text("hello world");
        h.assert_mode(Mode::Normal);
    }

    #[test]
    fn harness_smoke_motion_and_delete() {
        let mut h = Harness::with_text("hello world");
        h.keys("dw");
        h.assert_text("world");
    }

    #[test]
    fn harness_cmd_writes_cmdline() {
        let mut h = Harness::with_text("foo");
        h.keys(":noh");
        // pressing ':' enters Command mode and accumulates the rest in cmdline.
        h.assert_mode(Mode::Command);
        assert_eq!(h.cmdline(), "noh");
    }
}
