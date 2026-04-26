//! Arrow keys mirror hjkl: motion in Normal/Visual (composes with operators),
//! cursor move in Insert.

#![allow(non_snake_case)]

use vix_tui::testing::Harness;

#[test]
fn right_arrow_moves_like_l_in_normal() {
    let mut h = Harness::with_text("hello\n");
    h.keys("<Right><Right>");
    assert_eq!(h.cursor(), (0, 2));
}

#[test]
fn down_arrow_moves_like_j_in_normal() {
    let mut h = Harness::with_text("aaa\nbbb\nccc\n");
    h.keys("<Down>");
    assert_eq!(h.cursor(), (1, 0));
}

#[test]
fn up_arrow_moves_like_k_in_normal() {
    let mut h = Harness::with_text("aaa\nbbb\n");
    h.keys("j<Up>");
    assert_eq!(h.cursor(), (0, 0));
}

#[test]
fn left_arrow_moves_like_h_in_normal() {
    let mut h = Harness::with_text("hello\n");
    h.keys("ll<Left>");
    assert_eq!(h.cursor(), (0, 1));
}

#[test]
fn arrow_composes_with_operator() {
    // d<Right> deletes one char forward, just like dl.
    let mut h = Harness::with_text("hello\n");
    h.keys("d<Right>");
    assert_eq!(h.text(), "ello\n");
}

#[test]
fn arrow_count_prefix_works() {
    let mut h = Harness::with_text("hello\n");
    h.keys("3<Right>");
    assert_eq!(h.cursor(), (0, 3));
}

#[test]
fn arrow_extends_visual_selection() {
    let mut h = Harness::with_text("hello\n");
    h.keys("v<Right><Right>d");
    // v starts at col 0, two Right arrows extend, d deletes the inclusive
    // 3-char selection.
    assert_eq!(h.text(), "lo\n");
}

#[test]
fn arrow_moves_cursor_in_insert_mode() {
    let mut h = Harness::with_text("hello\n");
    // Enter insert at start, type 'X', arrow-left over the X back to col 0,
    // type 'Y' before everything. Result: YXhello.
    h.keys("iX<Left>Y<Esc>");
    assert_eq!(h.text(), "YXhello\n");
}

#[test]
fn arrow_in_insert_breaks_dot_repeat_session() {
    // Vim parity: cursor move inside Insert mode breaks the recorded burst
    // so `.` only repeats text typed *after* the move.
    let mut h = Harness::with_text("abc\n");
    h.keys("iX<Right>Y<Esc>");
    // Buffer is now "XaYbc\n", cursor on 'Y' (col 2). `.` should repeat just
    // "Y" (the post-arrow text), not "XY".
    assert_eq!(h.text(), "XaYbc\n");
    h.keys(".");
    // Repeat re-inserts "Y" at the current position; we don't assert the
    // exact spot (that depends on enter_insert positioning), just that
    // exactly one new Y was added — i.e. X was NOT replayed.
    let count_y = h.text().matches('Y').count();
    let count_x = h.text().matches('X').count();
    assert_eq!(count_y, 2, "dot-repeat should re-insert the post-arrow Y");
    assert_eq!(count_x, 1, "dot-repeat must NOT replay the pre-arrow X");
}
