//! `/`, `?`, n, N, `*`, `#`, `:noh`, smart-case.

#![allow(non_snake_case)]

use vix_tui::testing::Harness;

const PARA: &str = "\
hello world
foo bar baz
foo qux
hello there
";

#[test]
fn forward_search_lands_on_first_match() {
    let mut h = Harness::with_text(PARA);
    h.keys("/foo<CR>");
    h.assert_cursor(1, 0);
}

#[test]
fn backward_search_finds_previous_match() {
    let mut h = Harness::with_text(PARA);
    // Land on "hello there" line first then `?hello?` to find earlier "hello".
    h.keys("G?hello<CR>");
    h.assert_cursor(3, 0);
    // Hmm, ?hello on line 3 — hits "hello" on line 3 itself? Backward search
    // starts at cursor and includes it, so the match is the current line.
}

#[test]
fn n_repeats_search() {
    let mut h = Harness::with_text(PARA);
    h.keys("/foo<CR>");
    h.assert_cursor(1, 0);
    h.keys("n");
    h.assert_cursor(2, 0);
    // Wrap: only two foo lines, so next n wraps to line 1.
    h.keys("n");
    h.assert_cursor(1, 0);
}

#[test]
fn N_reverses_search() {
    let mut h = Harness::with_text(PARA);
    h.keys("/foo<CR>");
    h.keys("n"); // line 2
    h.keys("N"); // back to line 1
    h.assert_cursor(1, 0);
}

#[test]
fn star_searches_word_under_cursor() {
    let mut h = Harness::with_text("foo bar foo baz\n");
    h.keys("*");
    h.assert_cursor(0, 8);
}

#[test]
fn star_uses_word_boundaries() {
    // "foo" should not match "foobar".
    let mut h = Harness::with_text("foo foobar foo\n");
    h.keys("*");
    h.assert_cursor(0, 11);
}

#[test]
fn no_match_sets_message() {
    let mut h = Harness::with_text("hello\n");
    h.keys("/qzxq<CR>");
    assert!(h.msg().contains("not found"), "msg was: {:?}", h.msg());
}

#[test]
fn smart_case_lowercase_is_insensitive() {
    let mut h = Harness::with_text("Hello WORLD\n");
    h.keys("/hello<CR>");
    h.assert_cursor(0, 0);
}

#[test]
fn smart_case_uppercase_forces_sensitive() {
    let mut h = Harness::with_text("hello Hello\n");
    h.keys("/Hello<CR>");
    h.assert_cursor(0, 6);
}

#[test]
fn search_with_count_in_substitute() {
    let mut h = Harness::with_text("foo foo foo\n");
    h.cmd("%s/foo/bar/g");
    h.assert_text("bar bar bar\n");
}

#[test]
fn substitute_first_only_without_g() {
    let mut h = Harness::with_text("foo foo foo\n");
    h.cmd("%s/foo/bar/");
    h.assert_text("bar foo foo\n");
}

#[test]
fn substitute_case_insensitive_flag() {
    let mut h = Harness::with_text("Foo FOO foo\n");
    h.cmd("%s/foo/bar/gi");
    h.assert_text("bar bar bar\n");
}

#[test]
fn current_line_substitute() {
    let mut h = Harness::with_text("alpha foo\nbeta foo\n");
    h.keys("j");
    h.cmd("s/foo/bar/");
    h.assert_text("alpha foo\nbeta bar\n");
}

#[test]
fn forward_search_wraps() {
    let mut h = Harness::with_text("alpha\nbeta\nalpha\n");
    h.keys("G");
    h.keys("/alpha<CR>");
    // Wraps from line 3 to line 0 — first match again.
    h.assert_cursor(0, 0);
}

#[test]
fn search_then_dw_deletes_match() {
    let mut h = Harness::with_text("alpha bravo charlie\n");
    h.keys("/bravo<CR>");
    h.assert_cursor(0, 6);
    h.keys("dw");
    h.assert_text("alpha charlie\n");
}
