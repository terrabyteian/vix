//! `.` repeat coverage. Each test makes a change, asserts the buffer state,
//! then issues `.` (sometimes with a count) and checks the replay.

use vix_tui::testing::Harness;

#[test]
fn dot_repeats_dw() {
    let mut h = Harness::with_text("foo bar baz qux\n");
    h.keys("dw");
    h.assert_text("bar baz qux\n");
    h.keys(".");
    h.assert_text("baz qux\n");
}

#[test]
fn dot_repeats_diw_after_motion() {
    let mut h = Harness::with_text("foo bar baz\n");
    h.keys("diw");
    h.assert_text(" bar baz\n");
    // Move forward and replay; the text-object resolves at the new cursor.
    h.keys("w.");
    // After `w` cursor is on 'b' of "bar"; `.` re-runs `diw` deleting "bar"
    // and leaving the surrounding spaces alone.
    h.assert_text("  baz\n");
}

#[test]
fn dot_repeats_x() {
    let mut h = Harness::with_text("abcdef\n");
    h.keys("x");
    h.assert_text("bcdef\n");
    h.keys("..");
    h.assert_text("def\n");
}

#[test]
fn dot_repeats_insert_burst() {
    let mut h = Harness::with_text("hello world\n");
    // Append " !" at end of line.
    h.keys("A !<Esc>");
    h.assert_text("hello world !\n");
    // Move to start of line and `.` should re-run the insert burst there.
    h.keys("0.");
    // `A` ("AfterLine") repositions to end-of-line every replay, so even
    // from col 0 the next " !" lands at end-of-line again.
    h.assert_text("hello world ! !\n");
}

#[test]
fn dot_repeats_change_object_with_typed_text() {
    let mut h = Harness::with_text("\"hello\" \"world\"\n");
    // Position on first opening quote and change inside; type "x".
    h.keys("f\"ci\"x<Esc>");
    h.assert_text("\"x\" \"world\"\n");
    // Move to the second pair of quotes and `.` — the full ci"x change
    // should replay (delete inner content, type "x").
    h.keys("f\"f\".");
    h.assert_text("\"x\" \"x\"\n");
}

#[test]
fn count_on_dot() {
    // Each `.` replays once; counts are honored on the original action, not
    // multiplied through `.`. Standard vim: `.` runs once.
    let mut h = Harness::with_text("a b c d e f\n");
    h.keys("dw");
    h.keys("...");
    h.assert_text("e f\n");
}

#[test]
fn dot_after_undo_replays_original() {
    let mut h = Harness::with_text("foo bar baz\n");
    h.keys("dw");
    h.assert_text("bar baz\n");
    h.keys("u");
    h.assert_text("foo bar baz\n");
    h.keys(".");
    h.assert_text("bar baz\n");
}

#[test]
fn lsp_edit_does_not_pollute_dot() {
    use vix_lsp::lsp_types::{Position, Range, TextEdit};
    let mut h = Harness::with_text("foo bar\n");
    h.keys("dw");
    h.assert_text("bar\n");
    // Apply an LSP-style edit directly.
    let edit = TextEdit {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
        },
        new_text: "qux ".into(),
    };
    h.editor.apply_text_edits(&[edit]);
    h.assert_text("qux bar\n");
    // `.` should still replay the original `dw`.
    h.keys("0.");
    h.assert_text("bar\n");
}

#[test]
fn dot_after_yy_p_repeats_paste() {
    let mut h = Harness::with_text("alpha\nbeta\n");
    h.keys("yyp");
    h.assert_text("alpha\nalpha\nbeta\n");
    h.keys(".");
    h.assert_text("alpha\nalpha\nalpha\nbeta\n");
}
