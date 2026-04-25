//! Insert-mode entry points: i/a/I/A/o/O, plus Backspace / Enter / Tab.

#![allow(non_snake_case)]

use vix_core::Mode;
use vix_tui::testing::Harness;

#[test]
fn i_inserts_at_cursor() {
    let mut h = Harness::with_text("world\n");
    h.keys("ihello <Esc>");
    h.assert_text("hello world\n");
}

#[test]
fn a_appends_after_cursor() {
    let mut h = Harness::with_text("ab\n");
    h.keys("a-X<Esc>");
    h.assert_text("a-Xb\n");
}

#[test]
fn capital_I_inserts_before_first_non_blank() {
    let mut h = Harness::with_text("    hello\n");
    h.keys("Ihi <Esc>");
    h.assert_text("    hi hello\n");
}

#[test]
fn capital_A_appends_at_eol() {
    let mut h = Harness::with_text("hi\n");
    h.keys("A there<Esc>");
    h.assert_text("hi there\n");
}

#[test]
fn o_opens_line_below() {
    let mut h = Harness::with_text("alpha\nbeta\n");
    h.keys("onew<Esc>");
    h.assert_text("alpha\nnew\nbeta\n");
}

#[test]
fn capital_O_opens_line_above() {
    let mut h = Harness::with_text("alpha\nbeta\n");
    h.keys("Onew<Esc>");
    h.assert_text("new\nalpha\nbeta\n");
}

#[test]
fn backspace_deletes_typed_char() {
    let mut h = Harness::with_text("");
    h.keys("ihello<BS><Esc>");
    h.assert_text("hell");
}

#[test]
fn enter_inserts_newline() {
    let mut h = Harness::with_text("");
    h.keys("ifoo<CR>bar<Esc>");
    h.assert_text("foo\nbar");
}

#[test]
fn tab_inserts_four_spaces() {
    let mut h = Harness::with_text("");
    h.keys("i<Tab>x<Esc>");
    h.assert_text("    x");
}

#[test]
fn esc_returns_to_normal_mode() {
    let mut h = Harness::with_text("");
    h.keys("ifoo<Esc>");
    h.assert_mode(Mode::Normal);
}

#[test]
fn ctrl_c_in_insert_returns_to_normal() {
    let mut h = Harness::with_text("");
    h.keys("ifoo<C-c>");
    h.assert_mode(Mode::Normal);
    h.assert_text("foo");
}

#[test]
fn insert_burst_is_one_undo_unit() {
    let mut h = Harness::with_text("");
    h.keys("ihello world<Esc>");
    h.assert_text("hello world");
    h.keys("u");
    h.assert_text("");
}

#[test]
fn cursor_moves_left_on_leaving_insert() {
    // Vim: leaving insert mode (unless at col 0) moves cursor 1 left.
    let mut h = Harness::with_text("");
    h.keys("iabc<Esc>");
    h.assert_cursor(0, 2); // landed on 'c', not after it
}
