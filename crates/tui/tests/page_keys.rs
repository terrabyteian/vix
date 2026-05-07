//! PageUp / PageDown jump the cursor by one screen (viewport height − 2,
//! vim's `<C-f>`/`<C-b>` convention).

#![allow(non_snake_case)]

use ratatui::layout::Rect;
use vix_tui::testing::Harness;

/// 80×10 viewport — page step is 10 − 2 = 8 lines.
fn small_geom(h: &mut Harness) {
    h.set_render_geometry(Rect::new(0, 0, 80, 10), 5);
}

fn long_buffer(n: usize) -> String {
    (0..n).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn page_down_jumps_one_screen_in_normal() {
    let mut h = Harness::with_text(&long_buffer(50));
    small_geom(&mut h);
    assert_eq!(h.cursor().0, 0);
    h.keys("<PageDown>");
    assert_eq!(h.cursor().0, 8);
}

#[test]
fn page_up_jumps_one_screen_back() {
    let mut h = Harness::with_text(&long_buffer(50));
    small_geom(&mut h);
    h.keys("20G");
    assert_eq!(h.cursor().0, 19);
    h.keys("<PageUp>");
    assert_eq!(h.cursor().0, 11);
}

#[test]
fn page_down_clamps_at_eof() {
    // PageDown past EOF should land on the last line — same behavior as `G`.
    let mut h = Harness::with_text(&long_buffer(5));
    small_geom(&mut h);
    h.keys("G");
    let last = h.cursor().0;
    let mut h2 = Harness::with_text(&long_buffer(5));
    small_geom(&mut h2);
    h2.keys("<PageDown>");
    assert_eq!(h2.cursor().0, last);
}

#[test]
fn page_up_clamps_at_top() {
    let mut h = Harness::with_text(&long_buffer(5));
    small_geom(&mut h);
    h.keys("<PageUp>");
    assert_eq!(h.cursor().0, 0);
}

#[test]
fn page_down_extends_visual_selection() {
    let mut h = Harness::with_text(&long_buffer(50));
    small_geom(&mut h);
    h.keys("V<PageDown>d");
    // Started linewise-visual at line 0, extended 8 lines, deleted 9 lines
    // (inclusive). The remaining buffer should start at the original "line 9".
    let text = h.text();
    let first = text.lines().next().unwrap_or("");
    assert_eq!(first, "line 9");
}

#[test]
fn page_down_works_in_insert_mode() {
    let mut h = Harness::with_text(&long_buffer(50));
    small_geom(&mut h);
    h.keys("i<PageDown>");
    assert_eq!(h.cursor().0, 8);
}

#[test]
fn page_step_falls_back_when_no_render() {
    // No set_render_geometry → fallback step of 10.
    let mut h = Harness::with_text(&long_buffer(50));
    h.keys("<PageDown>");
    assert_eq!(h.cursor().0, 10);
}
