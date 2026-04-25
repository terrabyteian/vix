//! Text-object tests: iw/aw, i"/a", i'/a', i(/a(, i[/a[, i{/a{,
//! plus quote-escape and nesting edge cases.

use vix_tui::testing::Harness;

#[test]
fn diw_inner_word() {
    let mut h = Harness::with_text("foo bar baz\n");
    h.keys("wdiw");
    h.assert_text("foo  baz\n");
}

#[test]
fn daw_around_word_takes_trailing_space() {
    let mut h = Harness::with_text("foo bar baz\n");
    h.keys("wdaw");
    h.assert_text("foo baz\n");
}

#[test]
fn ciw_then_insert() {
    let mut h = Harness::with_text("foo bar baz\n");
    h.keys("wciwQUX<Esc>");
    h.assert_text("foo QUX baz\n");
}

#[test]
fn di_double_quote() {
    let mut h = Harness::with_text("let s = \"hello\"\n");
    // f" lands on opening quote.
    h.keys("f\"di\"");
    h.assert_text("let s = \"\"\n");
}

#[test]
fn da_double_quote_includes_quotes() {
    let mut h = Harness::with_text("let s = \"hello\"\n");
    h.keys("f\"da\"");
    h.assert_text("let s = \n");
}

#[test]
fn di_single_quote() {
    let mut h = Harness::with_text("name = 'alice'\n");
    h.keys("f'di'");
    h.assert_text("name = ''\n");
}

#[test]
fn quote_escape_aware() {
    // The pair we want is the OUTER quotes; the inner \" is escaped.
    let mut h = Harness::with_text("a = \"x\\\"y\"\n");
    // Position cursor on first " by jumping to it.
    h.keys("f\"di\"");
    h.assert_text("a = \"\"\n");
}

#[test]
fn di_paren_inner() {
    let mut h = Harness::with_text("call(foo, bar)\n");
    h.keys("f(di(");
    h.assert_text("call()\n");
}

#[test]
fn da_paren_around() {
    let mut h = Harness::with_text("call(foo, bar)\n");
    h.keys("f(da(");
    h.assert_text("call\n");
}

#[test]
fn nested_paren_inner_innermost() {
    let mut h = Harness::with_text("a(b(c)d)e\n");
    // Land on 'c' (col 4).
    h.keys("4ldi(");
    h.assert_text("a(b()d)e\n");
}

#[test]
fn di_brace_inner() {
    let mut h = Harness::with_text("fn foo() { return 1; }\n");
    h.keys("f{di{");
    h.assert_text("fn foo() {}\n");
}

#[test]
fn di_bracket_inner() {
    let mut h = Harness::with_text("xs = [1, 2, 3]\n");
    h.keys("f[di[");
    h.assert_text("xs = []\n");
}

#[test]
fn ci_paren_enters_insert() {
    let mut h = Harness::with_text("call(old)\n");
    h.keys("f(ci(new<Esc>");
    h.assert_text("call(new)\n");
}

#[test]
fn yi_quote_yanks_inner() {
    let mut h = Harness::with_text("s = \"hello\"\n");
    h.keys("f\"yi\"");
    let (text, _) = h.register();
    assert_eq!(text, "hello");
}

#[test]
fn yi_word() {
    let mut h = Harness::with_text("foo bar baz\n");
    h.keys("wyiw");
    let (text, _) = h.register();
    assert_eq!(text, "bar");
}

#[test]
fn diw_on_punctuation_cluster() {
    // Inner-word on a non-word char selects just that char (vim's behavior).
    let mut h = Harness::with_text("a + b\n");
    h.keys("lldiw");
    h.assert_text("a  b\n");
}

#[test]
fn ciw_at_buffer_end() {
    let mut h = Harness::with_text("hello\n");
    h.keys("ciwworld<Esc>");
    h.assert_text("world\n");
}
