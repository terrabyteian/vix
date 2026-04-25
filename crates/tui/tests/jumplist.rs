//! Jump-list: go/gi primary bindings, Ctrl-O/Ctrl-I/Tab fallback,
//! push points (G, gg, /, *, buffer switch), forward-after-back semantics.

#![allow(non_snake_case)]

use vix_tui::testing::Harness;

const PARA: &str = "\
alpha
beta
gamma
delta
epsilon
zeta
";

#[test]
fn G_then_go_returns_to_origin() {
    let mut h = Harness::with_text(PARA);
    h.assert_cursor(0, 0);
    h.keys("G");
    // G lands on the empty trailing line.
    h.assert_cursor(6, 0);
    // `go` (g-prefix jump-back, doesn't conflict with zellij Ctrl-O).
    h.keys("go");
    h.assert_cursor(0, 0);
}

#[test]
fn go_then_gi_round_trip() {
    let mut h = Harness::with_text(PARA);
    h.keys("G");
    let after_G = h.cursor();
    h.keys("go");
    h.assert_cursor(0, 0);
    h.keys("gi");
    h.assert_cursor(after_G.0, after_G.1);
}

#[test]
fn ctrl_o_fallback_works() {
    let mut h = Harness::with_text(PARA);
    h.keys("G");
    h.keys("<C-o>");
    h.assert_cursor(0, 0);
}

#[test]
fn tab_is_jump_forward_fallback() {
    let mut h = Harness::with_text(PARA);
    h.keys("G");
    h.keys("go");
    h.keys("<Tab>");
    // Forward after go should return to the line G landed on.
    h.assert_cursor(6, 0);
}

#[test]
fn gg_pushes_a_jump() {
    let mut h = Harness::with_text(PARA);
    h.keys("G");      // push (0, 0); land at (6, 0)
    h.keys("3G");     // push (6, 0); land somewhere on line 2
    let mid = h.cursor();
    h.keys("gg");     // push mid; land at (0, 0)
    h.assert_cursor(0, 0);
    h.keys("go");     // back to mid
    h.assert_cursor(mid.0, mid.1);
}

#[test]
fn search_pushes_a_jump() {
    let mut h = Harness::with_text(PARA);
    h.keys("/gamma<CR>");
    h.assert_cursor(2, 0);
    h.keys("go");
    h.assert_cursor(0, 0);
}

#[test]
fn star_pushes_a_jump() {
    let mut h = Harness::with_text("foo bar foo baz\n");
    h.keys("*");
    h.assert_cursor(0, 8);
    h.keys("go");
    h.assert_cursor(0, 0);
}

#[test]
fn jumps_picker_opens() {
    let mut h = Harness::with_text(PARA);
    h.keys("G");
    h.cmd("jumps");
    assert!(h.picker_open());
}

#[test]
fn no_back_at_top_sets_message() {
    let mut h = Harness::with_text(PARA);
    h.keys("go");
    assert!(h.msg().contains("top of jump list"));
}

#[test]
fn no_forward_at_tip_sets_message() {
    let mut h = Harness::with_text(PARA);
    h.keys("G");
    h.keys("gi");
    assert!(h.msg().contains("bottom of jump list"));
}

#[test]
fn jumplist_state_after_round_trip() {
    let mut h = Harness::with_text(PARA);
    h.keys("G");
    h.keys("go");
    // After going back from tip, vim stashes the current cursor so gi can
    // return — list should have at least one entry.
    assert!(!h.jump_list().is_empty());
}
