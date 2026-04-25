//! Undo / redo: u undoes a change, Ctrl-R redoes, insert sessions are one
//! undo unit, undo restores cursor.

#![allow(non_snake_case)]

use vix_tui::testing::Harness;

#[test]
fn u_undoes_dw() {
    let mut h = Harness::with_text("foo bar\n");
    h.keys("dw");
    h.assert_text("bar\n");
    h.keys("u");
    h.assert_text("foo bar\n");
}

#[test]
fn ctrl_r_redoes() {
    let mut h = Harness::with_text("foo bar\n");
    h.keys("dw");
    h.keys("u");
    h.assert_text("foo bar\n");
    h.keys("<C-r>");
    h.assert_text("bar\n");
}

#[test]
fn insert_session_is_one_undo() {
    let mut h = Harness::with_text("");
    h.keys("ihello world<Esc>");
    h.assert_text("hello world");
    h.keys("u");
    h.assert_text("");
}

#[test]
fn multiple_changes_undo_individually() {
    let mut h = Harness::with_text("alpha bravo charlie\n");
    h.keys("dw"); // delete "alpha "
    h.keys("dw"); // delete "bravo "
    h.assert_text("charlie\n");
    h.keys("u");
    h.assert_text("bravo charlie\n");
    h.keys("u");
    h.assert_text("alpha bravo charlie\n");
}

#[test]
fn undo_at_clean_buffer_is_noop() {
    let mut h = Harness::with_text("hello\n");
    h.keys("u");
    h.assert_text("hello\n");
}

#[test]
fn redo_after_new_change_drops_redo_stack() {
    let mut h = Harness::with_text("alpha bravo charlie\n");
    h.keys("dw");
    h.keys("u");
    // New change clears the redo stack.
    h.keys("xx");
    h.keys("<C-r>");
    // Should NOT bring back the original `dw`.
    assert!(!h.text().contains("alpha"));
}

#[test]
fn undo_restores_cursor_position() {
    let mut h = Harness::with_text("foo bar baz\n");
    h.keys("w"); // cursor at col 4
    h.keys("dw"); // delete "bar ", cursor at col 4 still
    h.keys("u"); // undo — cursor should restore to where it was before dw.
    let (line, _col) = h.cursor();
    assert_eq!(line, 0);
}

#[test]
fn yank_does_not_consume_undo() {
    let mut h = Harness::with_text("hello\n");
    h.keys("yy");
    h.keys("u");
    // Yank made no change, so undo is a no-op.
    h.assert_text("hello\n");
}

#[test]
fn lsp_edit_is_undoable() {
    use vix_lsp::lsp_types::{Position, Range, TextEdit};
    let mut h = Harness::with_text("hello\n");
    let edit = TextEdit {
        range: Range {
            start: Position { line: 0, character: 0 },
            end: Position { line: 0, character: 5 },
        },
        new_text: "world".into(),
    };
    h.editor.apply_text_edits(&[edit]);
    h.assert_text("world\n");
    h.keys("u");
    h.assert_text("hello\n");
}

#[test]
fn change_burst_is_one_undo() {
    let mut h = Harness::with_text("foo bar\n");
    h.keys("ciwQUX<Esc>");
    h.assert_text("QUX bar\n");
    h.keys("u");
    h.assert_text("foo bar\n");
}
