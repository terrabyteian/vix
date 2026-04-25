//! Operator tests: d / c / y / ~ / gu / gU / < / > over motions, line-wise
//! variants (dd / yy / cc), counts, and the resulting register state.

#![allow(non_snake_case)]

use vix_core::Mode;
use vix_tui::testing::Harness;

#[test]
fn dw_yanks_to_register_charwise() {
    let mut h = Harness::with_text("foo bar baz\n");
    h.keys("dw");
    h.assert_text("bar baz\n");
    let (text, linewise) = h.register();
    assert_eq!(text, "foo ");
    assert!(!linewise);
}

#[test]
fn dd_yanks_linewise() {
    let mut h = Harness::with_text("alpha\nbeta\ngamma\n");
    h.keys("dd");
    h.assert_text("beta\ngamma\n");
    let (text, linewise) = h.register();
    assert_eq!(text, "alpha\n");
    assert!(linewise);
}

#[test]
fn count_dd_deletes_n_lines() {
    let mut h = Harness::with_text("a\nb\nc\nd\ne\n");
    h.keys("3dd");
    h.assert_text("d\ne\n");
}

#[test]
fn yy_then_p_pastes_after() {
    let mut h = Harness::with_text("alpha\nbeta\n");
    h.keys("yyp");
    h.assert_text("alpha\nalpha\nbeta\n");
    h.assert_cursor(1, 0);
}

#[test]
fn yy_then_P_pastes_before() {
    let mut h = Harness::with_text("alpha\nbeta\n");
    h.keys("yyP");
    h.assert_text("alpha\nalpha\nbeta\n");
    h.assert_cursor(0, 0);
}

#[test]
fn yiw_yanks_inner_word() {
    let mut h = Harness::with_text("foo bar baz\n");
    h.keys("wyiw");
    let (text, _) = h.register();
    assert_eq!(text, "bar");
    // Cursor should remain unchanged after yank.
    h.assert_cursor(0, 4);
}

#[test]
fn dG_deletes_to_end() {
    let mut h = Harness::with_text("alpha\nbeta\ngamma\n");
    h.keys("dG");
    // dG removes from current line through end of file (linewise).
    h.assert_text("");
}

#[test]
fn d_dollar_to_end_of_line() {
    let mut h = Harness::with_text("hello world\n");
    h.keys("wd$");
    h.assert_text("hello \n");
}

#[test]
fn x_deletes_char_under_cursor() {
    let mut h = Harness::with_text("abcdef\n");
    h.keys("x");
    h.assert_text("bcdef\n");
}

#[test]
fn count_x_deletes_chars() {
    let mut h = Harness::with_text("abcdef\n");
    h.keys("3x");
    h.assert_text("def\n");
}

#[test]
fn capital_X_deletes_before_cursor() {
    let mut h = Harness::with_text("abcdef\n");
    h.keys("llX");
    h.assert_text("acdef\n");
}

#[test]
fn cc_changes_whole_line() {
    let mut h = Harness::with_text("alpha\nbeta\n");
    h.keys("ccnew<Esc>");
    h.assert_text("new\nbeta\n");
    h.assert_mode(Mode::Normal);
}

#[test]
fn tilde_swaps_case() {
    let mut h = Harness::with_text("hello\n");
    h.keys("~");
    h.assert_text("Hello\n");
    h.keys("~");
    // Cursor advances after each `~`, so subsequent ~ flips next char.
    h.assert_text("HEllo\n");
}

#[test]
fn gu_iw_lowercases_word() {
    let mut h = Harness::with_text("Foo BAR baz\n");
    // Position on "BAR" then `guiw`.
    h.keys("wguiw");
    h.assert_text("Foo bar baz\n");
}

#[test]
fn gU_iw_uppercases_word() {
    let mut h = Harness::with_text("foo bar baz\n");
    h.keys("wgUiw");
    h.assert_text("foo BAR baz\n");
}

#[test]
fn shift_right_indents_line() {
    let mut h = Harness::with_text("foo\nbar\n");
    h.keys(">>");
    h.assert_text("    foo\nbar\n");
}

#[test]
fn shift_left_outdents_line() {
    let mut h = Harness::with_text("    foo\nbar\n");
    h.keys("<<");
    h.assert_text("foo\nbar\n");
}

#[test]
fn yank_does_not_set_dirty() {
    let mut h = Harness::with_text("hello\n");
    h.keys("yy");
    assert!(!h.dirty(), "yank shouldn't dirty the buffer");
}

#[test]
fn delete_sets_dirty() {
    let mut h = Harness::with_text("hello\n");
    h.keys("dw");
    assert!(h.dirty());
}

#[test]
fn yank_dollar_yanks_to_end_of_line() {
    let mut h = Harness::with_text("hello world\nnext\n");
    h.keys("y$");
    let (text, linewise) = h.register();
    assert_eq!(text, "hello world");
    assert!(!linewise);
}

#[test]
fn count_yy_yanks_n_lines() {
    let mut h = Harness::with_text("a\nb\nc\nd\n");
    h.keys("2yy");
    let (text, linewise) = h.register();
    assert_eq!(text, "a\nb\n");
    assert!(linewise);
}
