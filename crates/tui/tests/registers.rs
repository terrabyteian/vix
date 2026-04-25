//! Unnamed register flow: yy/p/P, dd/p, charwise vs linewise paste positions.

#![allow(non_snake_case)]

use vix_tui::testing::Harness;

#[test]
fn yy_charwise_register_metadata() {
    let mut h = Harness::with_text("hello\nworld\n");
    h.keys("yy");
    let (text, linewise) = h.register();
    assert_eq!(text, "hello\n");
    assert!(linewise);
}

#[test]
fn yiw_is_charwise() {
    let mut h = Harness::with_text("foo bar\n");
    h.keys("yiw");
    let (text, linewise) = h.register();
    assert_eq!(text, "foo");
    assert!(!linewise);
}

#[test]
fn dd_then_p_pastes_below() {
    let mut h = Harness::with_text("alpha\nbeta\ngamma\n");
    h.keys("dd"); // active line is now "beta"
    h.keys("p");  // paste below current line
    h.assert_text("beta\nalpha\ngamma\n");
}

#[test]
fn dd_then_capital_P_pastes_above() {
    let mut h = Harness::with_text("alpha\nbeta\ngamma\n");
    h.keys("dd");
    h.keys("P");
    // After dd buffer is "beta\ngamma\n", then P pastes "alpha\n" above current
    h.assert_text("alpha\nbeta\ngamma\n");
}

#[test]
fn charwise_p_pastes_after_cursor() {
    let mut h = Harness::with_text("ab\n");
    h.keys("yl"); // yank single char 'a'
    h.keys("p");  // paste after → ab → a a b? Actually paste "a" after 'a' → "aab\n"
    h.assert_text("aab\n");
}

#[test]
fn charwise_capital_P_pastes_before_cursor() {
    let mut h = Harness::with_text("bc\n");
    h.keys("yl"); // yank 'b'
    h.keys("P");
    h.assert_text("bbc\n");
}

#[test]
fn yank_persists_through_motion() {
    let mut h = Harness::with_text("foo bar\n");
    h.keys("yiw");
    h.keys("w"); // motion shouldn't clear register
    let (text, _) = h.register();
    assert_eq!(text, "foo");
}

#[test]
fn delete_replaces_register_contents() {
    let mut h = Harness::with_text("hello world\n");
    h.keys("yiw");
    let (first, _) = h.register();
    assert_eq!(first, "hello");
    // Move to "world" deterministically and delete it.
    h.keys("fw");
    h.keys("diw");
    let (second, _) = h.register();
    assert_eq!(second, "world");
}

#[test]
fn p_with_count_pastes_n_times() {
    let mut h = Harness::with_text("xy\n");
    h.keys("yl");
    h.keys("3p");
    // Paste "x" three times after cursor.
    h.assert_text("xxxxy\n");
}
