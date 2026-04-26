//! Mouse: left-click positions cursor; drag enters/extends Visual;
//! scroll wheel adjusts the viewport.

#![allow(non_snake_case)]

use ratatui::layout::Rect;
use vix_core::Mode;
use vix_tui::testing::Harness;

/// 80×24 viewport, gutter 5 wide (3-digit line nums + diag glyph + space).
fn geom(h: &mut Harness) {
    h.set_render_geometry(Rect::new(0, 0, 80, 22), 5);
}

#[test]
fn click_sets_cursor_in_normal_mode() {
    let mut h = Harness::with_text("hello world\nsecond line\n");
    geom(&mut h);
    // Click on row 0, content col 6 (the 'w' of "world"). Add gutter offset.
    h.click(5 + 6, 0);
    assert_eq!(h.cursor(), (0, 6));
}

#[test]
fn click_on_second_line() {
    let mut h = Harness::with_text("hello world\nsecond line\n");
    geom(&mut h);
    h.click(5 + 3, 1);
    assert_eq!(h.cursor(), (1, 3));
}

#[test]
fn click_in_gutter_snaps_to_line_start() {
    let mut h = Harness::with_text("hello\n");
    geom(&mut h);
    // Column 2 is inside the gutter.
    h.click(2, 0);
    assert_eq!(h.cursor(), (0, 0));
}

#[test]
fn click_past_line_end_clamps() {
    let mut h = Harness::with_text("hi\n");
    geom(&mut h);
    h.click(5 + 50, 0);
    assert_eq!(h.cursor(), (0, 2));
}

#[test]
fn click_past_eof_does_nothing() {
    let mut h = Harness::with_text("only line\n");
    geom(&mut h);
    let before = h.cursor();
    h.click(5 + 1, 10);
    assert_eq!(h.cursor(), before);
}

#[test]
fn drag_starts_visual_selection() {
    let mut h = Harness::with_text("hello world\n");
    geom(&mut h);
    h.click(5, 0);
    h.drag(5 + 5, 0);
    assert_eq!(h.mode(), Mode::Visual);
    // Yank to inspect the selection.
    h.keys("y");
    assert_eq!(h.register().0, "hello ");
}

#[test]
fn click_in_visual_extends_selection() {
    let mut h = Harness::with_text("abcdef\n");
    geom(&mut h);
    h.keys("v"); // anchor at col 0
    h.click(5 + 3_u16, 0);
    h.keys("y");
    assert_eq!(h.register().0, "abcd");
}

#[test]
fn scroll_down_advances_cursor() {
    let big: String = (0..50).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::with_text(&big);
    geom(&mut h);
    assert_eq!(h.cursor().0, 0);
    h.scroll_down();
    // Cursor moves down 1; ensure_cursor_visible drags view_top along on render.
    assert_eq!(h.cursor().0, 1);
}

#[test]
fn scroll_up_clamps_to_buffer_top() {
    let mut h = Harness::with_text("a\nb\nc\n");
    geom(&mut h);
    h.scroll_up();
    assert_eq!(h.cursor().0, 0);
}

#[test]
fn click_with_no_geometry_is_noop() {
    // No set_render_geometry: harness has never "rendered". Click is a noop.
    let mut h = Harness::with_text("hello\n");
    let before = h.cursor();
    h.click(10, 0);
    assert_eq!(h.cursor(), before);
}
