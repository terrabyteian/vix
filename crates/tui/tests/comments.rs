//! `gc` line-comment toggle: visual-mode `gc` and normal-mode `gcc`.

use vix_core::Mode;
use vix_tui::testing::Harness;

#[test]
fn gcc_comments_current_line_rust() {
    let mut h = Harness::with_text_and_path("fn main() {}\n", "scratch.rs");
    h.keys("gcc");
    assert_eq!(h.text(), "// fn main() {}\n");
}

#[test]
fn gcc_uncomments_already_commented_line() {
    let mut h = Harness::with_text_and_path("// fn main() {}\n", "scratch.rs");
    h.keys("gcc");
    assert_eq!(h.text(), "fn main() {}\n");
}

#[test]
fn visual_gc_comments_selection() {
    let mut h = Harness::with_text_and_path("a\nb\nc\n", "scratch.rs");
    h.keys("Vjjgc");
    assert_eq!(h.text(), "// a\n// b\n// c\n");
    h.assert_mode(Mode::Normal);
}

#[test]
fn visual_gc_round_trips() {
    let mut h = Harness::with_text_and_path("a\nb\nc\n", "scratch.rs");
    h.keys("Vjjgc");
    h.keys("ggVjjgc");
    assert_eq!(h.text(), "a\nb\nc\n");
}

#[test]
fn gc_uses_min_indent_so_inner_lines_align() {
    // Min indent across non-blank lines is 2 (the middle line). The comment
    // marker lands at column 2 on every line; preceding indent is preserved
    // and trailing indent becomes part of the commented payload.
    let mut h = Harness::with_text_and_path("    one\n  two\n      three\n", "scratch.rs");
    h.keys("Vjjgc");
    assert_eq!(h.text(), "  //   one\n  // two\n  //     three\n");
}

#[test]
fn gc_python_uses_hash() {
    let mut h = Harness::with_text_and_path("x = 1\ny = 2\n", "scratch.py");
    h.keys("Vjgc");
    assert_eq!(h.text(), "# x = 1\n# y = 2\n");
}

#[test]
fn gc_skips_blank_lines_when_commenting() {
    let mut h = Harness::with_text_and_path("a\n\nb\n", "scratch.rs");
    h.keys("Vjjgc");
    assert_eq!(h.text(), "// a\n\n// b\n");
}

#[test]
fn gc_uncomments_only_when_all_non_blank_are_commented() {
    let mut h = Harness::with_text_and_path("// a\nb\n", "scratch.rs");
    h.keys("Vjgc");
    assert_eq!(h.text(), "// // a\n// b\n");
}

#[test]
fn gc_no_op_on_unsupported_language() {
    let mut h = Harness::with_text_and_path("{\n  \"a\": 1\n}\n", "scratch.json");
    h.keys("Vjjgc");
    assert_eq!(h.text(), "{\n  \"a\": 1\n}\n");
}

#[test]
fn gc_undo_restores_in_one_step() {
    let mut h = Harness::with_text_and_path("a\nb\nc\n", "scratch.rs");
    h.keys("Vjjgc");
    assert_eq!(h.text(), "// a\n// b\n// c\n");
    h.keys("u");
    assert_eq!(h.text(), "a\nb\nc\n");
}
