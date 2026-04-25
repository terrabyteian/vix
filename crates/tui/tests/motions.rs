//! Cursor-motion tests driven through the harness. Each case is a small
//! table-style assertion: known buffer, send keys, check cursor.

#![allow(non_snake_case)]

use vix_core::Mode;
use vix_tui::testing::Harness;

const HELLO: &str = "hello world\n";

const PARA: &str = "\
hello world
foo bar baz
the quick fox
";

#[test]
fn h_l_basic() {
    let mut h = Harness::with_text(HELLO);
    h.keys("l");
    h.assert_cursor(0, 1);
    h.keys("ll");
    h.assert_cursor(0, 3);
    h.keys("h");
    h.assert_cursor(0, 2);
}

#[test]
fn count_l() {
    let mut h = Harness::with_text(HELLO);
    h.keys("5l");
    h.assert_cursor(0, 5);
}

#[test]
fn dollar_and_zero() {
    let mut h = Harness::with_text(HELLO);
    h.keys("$");
    h.assert_cursor(0, 10);
    h.keys("0");
    h.assert_cursor(0, 0);
}

#[test]
fn word_forward() {
    let mut h = Harness::with_text(HELLO);
    h.keys("w");
    h.assert_cursor(0, 6);
}

#[test]
fn word_backward() {
    let mut h = Harness::with_text(HELLO);
    h.keys("$b");
    h.assert_cursor(0, 6);
    h.keys("b");
    h.assert_cursor(0, 0);
}

#[test]
fn word_end() {
    let mut h = Harness::with_text(HELLO);
    h.keys("e");
    h.assert_cursor(0, 4);
    h.keys("e");
    h.assert_cursor(0, 10);
}

#[test]
fn j_k_lines() {
    let mut h = Harness::with_text(PARA);
    h.keys("j");
    h.assert_cursor(1, 0);
    h.keys("j");
    h.assert_cursor(2, 0);
    h.keys("k");
    h.assert_cursor(1, 0);
}

#[test]
fn gg_and_G() {
    let mut h = Harness::with_text(PARA);
    h.keys("G");
    // ropey treats the trailing newline as opening an empty line 3; G lands
    // on `len_lines() - 1`, i.e. that empty line.
    h.assert_cursor(3, 0);
    h.keys("gg");
    h.assert_cursor(0, 0);
}

#[test]
fn find_char_forward() {
    let mut h = Harness::with_text(HELLO);
    h.keys("fw");
    h.assert_cursor(0, 6);
    h.keys(";");
    // No second 'w' on this line — cursor stays.
    h.assert_cursor(0, 6);
}

#[test]
fn till_char_forward() {
    let mut h = Harness::with_text(HELLO);
    h.keys("tw");
    h.assert_cursor(0, 5);
}

#[test]
fn find_char_backward() {
    let mut h = Harness::with_text(HELLO);
    h.keys("$Fo");
    // F finds previous 'o' — there's one in "world" at col 7.
    h.assert_cursor(0, 7);
}

#[test]
fn count_w() {
    let mut h = Harness::with_text("one two three four\n");
    h.keys("3w");
    h.assert_cursor(0, 14);
}

#[test]
fn match_bracket() {
    let mut h = Harness::with_text("fn main() { let x = (1 + 2); }\n");
    // From col 0, `%` finds the next bracket on the line and jumps to its
    // match. First bracket is '(' at col 7; its mate ')' is at col 8.
    h.keys("%");
    h.assert_cursor(0, 8);
}

#[test]
fn dw_deletes_word() {
    let mut h = Harness::with_text(HELLO);
    h.keys("dw");
    h.assert_text("world\n");
    h.assert_cursor(0, 0);
}

#[test]
fn dd_deletes_line() {
    let mut h = Harness::with_text(PARA);
    h.keys("dd");
    h.assert_text("foo bar baz\nthe quick fox\n");
}

#[test]
fn yy_p_duplicates_line() {
    let mut h = Harness::with_text(PARA);
    h.keys("yyp");
    h.assert_text("hello world\nhello world\nfoo bar baz\nthe quick fox\n");
    h.assert_cursor(1, 0);
}

#[test]
fn search_forward_then_n() {
    let mut h = Harness::with_text(PARA);
    h.keys("/bar<CR>");
    // Search lands on 'b' of "bar" — line 1 col 4.
    h.assert_cursor(1, 4);
    h.assert_mode(Mode::Normal);
}

#[test]
fn star_searches_word_under_cursor() {
    let mut h = Harness::with_text("foo bar foo baz\n");
    // Cursor on first "foo" — `*` jumps to the next "foo".
    h.keys("*");
    h.assert_cursor(0, 8);
}

#[test]
fn visual_then_d_deletes_selection() {
    let mut h = Harness::with_text("hello world\n");
    // v anchors at 0 and `ll` moves head to col 2 — selection is chars 0..=2.
    h.keys("vlld");
    h.assert_text("lo world\n");
}

#[test]
fn dot_repeats_last_change() {
    let mut h = Harness::with_text("foo bar baz\n");
    h.keys("dw");
    h.assert_text("bar baz\n");
    h.keys(".");
    h.assert_text("baz\n");
}

#[test]
fn count_dot() {
    let mut h = Harness::with_text("one two three four five\n");
    h.keys("dw");
    h.keys("..");
    h.assert_text("four five\n");
}

#[test]
fn change_word_enters_insert() {
    // Vim's `cw` is special-cased to act like `ce` — it does NOT consume
    // the trailing whitespace.
    let mut h = Harness::with_text("foo bar\n");
    h.keys("cwhi<Esc>");
    h.assert_text("hi bar\n");
    h.assert_mode(Mode::Normal);
}

#[test]
fn text_object_change_inside_quotes() {
    let mut h = Harness::with_text("let s = \"hello\"\n");
    // Position the cursor on the opening quote first; `ci"` requires the
    // cursor to be inside (or on the boundary of) the quoted region.
    h.keys("f\"ci\"world<Esc>");
    h.assert_text("let s = \"world\"\n");
}

#[test]
fn text_object_delete_around_paren() {
    let mut h = Harness::with_text("call(foo, bar)\n");
    // Land on the open paren first.
    h.keys("f(da(");
    h.assert_text("call\n");
}

#[test]
fn ex_substitute_global() {
    let mut h = Harness::with_text("foo bar foo baz\n");
    h.cmd("%s/foo/qux/g");
    h.assert_text("qux bar qux baz\n");
}
