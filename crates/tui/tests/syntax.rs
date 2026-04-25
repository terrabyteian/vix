//! Syntax / language detection / symbol picker.

#![allow(non_snake_case)]

use std::path::PathBuf;
use vix_syntax::Language;
use vix_tui::testing::Harness;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // workspace
    p.push("tests/fixtures");
    p.push(name);
    p
}

#[test]
fn rust_file_attaches_rust_syntax() {
    let mut h = Harness::with_text_and_path(
        &std::fs::read_to_string(fixture("sample.rs")).unwrap(),
        fixture("sample.rs"),
    );
    assert_eq!(h.active_language(), Some(Language::Rust));
    let _ = &mut h; // silence unused-mut warning if added later.
}

#[test]
fn python_file_attaches_python_syntax() {
    let h = Harness::with_text_and_path(
        &std::fs::read_to_string(fixture("sample.py")).unwrap(),
        fixture("sample.py"),
    );
    assert_eq!(h.active_language(), Some(Language::Python));
}

#[test]
fn typescript_file_attaches_ts_syntax() {
    let h = Harness::with_text_and_path(
        &std::fs::read_to_string(fixture("sample.ts")).unwrap(),
        fixture("sample.ts"),
    );
    assert_eq!(h.active_language(), Some(Language::TypeScript));
}

#[test]
fn markdown_file_attaches_markdown_syntax() {
    let h = Harness::with_text_and_path(
        &std::fs::read_to_string(fixture("sample.md")).unwrap(),
        fixture("sample.md"),
    );
    assert_eq!(h.active_language(), Some(Language::Markdown));
}

#[test]
fn json_file_attaches_json_syntax() {
    let h = Harness::with_text_and_path(
        &std::fs::read_to_string(fixture("sample.json")).unwrap(),
        fixture("sample.json"),
    );
    assert_eq!(h.active_language(), Some(Language::Json));
}

#[test]
fn unknown_extension_has_no_syntax() {
    let h = Harness::with_text_and_path("plain text\n", "notes.unknownext");
    assert_eq!(h.active_language(), None);
}

#[test]
fn rust_symbols_pick_up_top_level_defs() {
    let h = Harness::with_text_and_path(
        &std::fs::read_to_string(fixture("sample.rs")).unwrap(),
        fixture("sample.rs"),
    );
    let names = h.symbol_names();
    assert!(names.iter().any(|n| n == "greet"));
    assert!(names.iter().any(|n| n == "Counter"));
    assert!(names.iter().any(|n| n == "main"));
}

#[test]
fn python_symbols_pick_up_classes_and_funcs() {
    let h = Harness::with_text_and_path(
        &std::fs::read_to_string(fixture("sample.py")).unwrap(),
        fixture("sample.py"),
    );
    let names = h.symbol_names();
    assert!(names.iter().any(|n| n == "Counter"));
    assert!(names.iter().any(|n| n == "greet"));
    assert!(names.iter().any(|n| n == "main"));
}

#[test]
fn symbols_picker_opens_when_language_known() {
    let mut h = Harness::with_text_and_path(
        &std::fs::read_to_string(fixture("sample.rs")).unwrap(),
        fixture("sample.rs"),
    );
    h.cmd("Symbols");
    assert!(h.picker_open());
}

#[test]
fn symbols_picker_refuses_without_language() {
    let mut h = Harness::with_text("no language\n");
    h.cmd("Symbols");
    assert!(h.msg().contains("no language"));
    assert!(!h.picker_open());
}
