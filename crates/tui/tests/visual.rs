//! Visual mode (v / V): extending selection, operators over selection,
//! charwise vs linewise differences, mode toggles, Esc cancels.

#![allow(non_snake_case)]

use vix_core::Mode;
use vix_tui::testing::Harness;

#[test]
fn v_enters_visual() {
    let mut h = Harness::with_text("hello\n");
    h.keys("v");
    h.assert_mode(Mode::Visual);
}

#[test]
fn V_enters_visual_line() {
    let mut h = Harness::with_text("hello\n");
    h.keys("V");
    h.assert_mode(Mode::VisualLine);
}

#[test]
fn esc_leaves_visual() {
    let mut h = Harness::with_text("hello\n");
    h.keys("v<Esc>");
    h.assert_mode(Mode::Normal);
}

#[test]
fn v_to_V_toggles_mode() {
    let mut h = Harness::with_text("hello\nworld\n");
    h.keys("vlV");
    h.assert_mode(Mode::VisualLine);
}

#[test]
fn V_to_v_toggles_mode() {
    let mut h = Harness::with_text("hello\nworld\n");
    h.keys("Vv");
    h.assert_mode(Mode::Visual);
}

#[test]
fn d_in_visual_deletes_selection() {
    let mut h = Harness::with_text("hello world\n");
    // v + 4 right extends selection to cover "hello".
    h.keys("v4ld");
    h.assert_text(" world\n");
    h.assert_mode(Mode::Normal);
}

#[test]
fn y_in_visual_yanks_to_register() {
    let mut h = Harness::with_text("hello world\n");
    h.keys("v4ly");
    let (text, linewise) = h.register();
    assert_eq!(text, "hello");
    assert!(!linewise);
    h.assert_mode(Mode::Normal);
}

#[test]
fn V_d_deletes_whole_line() {
    let mut h = Harness::with_text("alpha\nbeta\ngamma\n");
    h.keys("Vd");
    h.assert_text("beta\ngamma\n");
}

#[test]
fn V_y_yanks_linewise() {
    let mut h = Harness::with_text("alpha\nbeta\n");
    h.keys("Vy");
    let (text, linewise) = h.register();
    assert_eq!(text, "alpha\n");
    assert!(linewise);
}

#[test]
fn c_in_visual_changes_and_enters_insert() {
    let mut h = Harness::with_text("hello world\n");
    h.keys("v4lc");
    h.assert_mode(Mode::Insert);
    h.keys("HELLO<Esc>");
    h.assert_text("HELLO world\n");
}

#[test]
fn tilde_swaps_case_in_visual() {
    let mut h = Harness::with_text("hello\n");
    h.keys("v4l~");
    h.assert_text("HELLO\n");
}

#[test]
fn p_pastes_over_visual() {
    let mut h = Harness::with_text("alpha bravo\n");
    h.keys("yiw");
    let (yanked, _) = h.register();
    assert_eq!(yanked, "alpha");
    // Move to "bravo", select it, paste over. The yank should survive the
    // implicit delete that paste-over-visual does.
    h.keys("wviwp");
    h.assert_text("alpha alpha\n");
}

#[test]
fn V_extends_to_multiple_lines() {
    let mut h = Harness::with_text("a\nb\nc\n");
    h.keys("Vjd");
    h.assert_text("c\n");
}
