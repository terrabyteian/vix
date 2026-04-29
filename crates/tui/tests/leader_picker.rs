//! `<Space>` leader opens the unified Files/Grep picker. `<Tab>` toggles
//! submode preserving the query. Esc clears the query first, then closes.

#![allow(non_snake_case)]

use ratatui::layout::Rect;
use std::fs;
use vix_tui::testing::Harness;

/// Build a temp dir with a few files and chdir into it. Returns the dir so
/// the caller can drop it (we leak — tempdirs in /tmp).
fn setup_repo() -> std::path::PathBuf {
    let dir = tempdir();
    fs::write(dir.join("alpha.txt"), "hello world\nthe quick brown fox\n").unwrap();
    fs::write(dir.join("beta.txt"), "lorem ipsum dolor\n").unwrap();
    fs::write(dir.join("gamma.rs"), "fn main() { println!(\"hello\"); }\n").unwrap();
    std::env::set_current_dir(&dir).unwrap();
    dir
}

fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vix-leader-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn space_f_opens_files_picker() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    assert!(!h.picker_open());
    h.keys("<Space>f");
    assert!(h.picker_open());
    assert_eq!(h.picker_kind(), Some("files"));
    assert_eq!(h.picker_query(), Some(""));
}

#[test]
fn space_g_opens_grep_picker() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>g");
    assert!(h.picker_open());
    assert_eq!(h.picker_kind(), Some("grep"));
}

#[test]
fn space_followed_by_unknown_does_nothing() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>x");
    assert!(!h.picker_open());
}

#[test]
fn esc_clears_pending_leader() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    // Press Space to arm the leader, then Esc — should NOT open a picker
    // when followed by 'f'. Instead 'f' is interpreted as the find-char
    // motion (which is pending — needs a target — but the picker stays
    // closed).
    h.keys("<Space><Esc>f");
    assert!(!h.picker_open());
}

#[test]
fn tab_toggles_files_to_grep_preserving_query() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>fhe");
    assert_eq!(h.picker_kind(), Some("files"));
    assert_eq!(h.picker_query(), Some("he"));
    h.keys("<Tab>");
    assert_eq!(h.picker_kind(), Some("grep"));
    assert_eq!(h.picker_query(), Some("he"));
}

#[test]
fn tab_toggles_grep_back_to_files_preserving_query() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>ghello<Tab>");
    assert_eq!(h.picker_kind(), Some("files"));
    assert_eq!(h.picker_query(), Some("hello"));
}

#[test]
fn esc_clears_query_first_then_closes() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>fabc");
    assert_eq!(h.picker_query(), Some("abc"));
    h.keys("<Esc>");
    // Picker still open, query cleared.
    assert!(h.picker_open());
    assert_eq!(h.picker_query(), Some(""));
    h.keys("<Esc>");
    // Now picker is closed.
    assert!(!h.picker_open());
}

#[test]
fn esc_with_empty_query_closes_immediately() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    assert_eq!(h.picker_query(), Some(""));
    h.keys("<Esc>");
    assert!(!h.picker_open());
}

#[test]
fn enter_switches_picker_to_browse_before_selecting() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.keys("<CR>");
    assert!(h.picker_open(), "first enter should enter browse mode");
    h.keys("<CR>");
    assert!(
        !h.picker_open(),
        "second enter should open the selected row"
    );
}

#[test]
fn jk_moves_selection_in_picker_browse_mode() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f<CR>");
    let before = h.editor.picker_selected_for_test();
    h.keys("j");
    assert_eq!(h.editor.picker_selected_for_test(), before + 1);
    h.keys("k");
    assert_eq!(h.editor.picker_selected_for_test(), before);
}

#[test]
fn jk_still_types_in_picker_input_mode() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>fjk");
    assert_eq!(h.picker_query(), Some("jk"));
}

#[test]
fn esc_returns_picker_from_browse_to_input() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>fal<CR><Esc>p");
    assert!(h.picker_open());
    assert_eq!(h.picker_query(), Some("alp"));
}

#[test]
fn ex_files_command_opens_unified_picker() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.cmd("Files");
    assert_eq!(h.picker_kind(), Some("files"));
    h.keys("<Tab>");
    assert_eq!(h.picker_kind(), Some("grep"));
}

#[test]
fn scroll_down_advances_picker_selection() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    assert!(h.picker_open());
    let before = h.editor.picker_selected_for_test();
    h.scroll_down();
    let after = h.editor.picker_selected_for_test();
    assert!(after > before, "scroll-down should move selection forward");
}

#[test]
fn scroll_up_after_scroll_down_returns_selection() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.scroll_down();
    h.scroll_down();
    let mid = h.editor.picker_selected_for_test();
    h.scroll_up();
    assert!(h.editor.picker_selected_for_test() < mid);
}

#[test]
fn first_click_focuses_second_click_activates() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    assert!(h.picker_open());
    h.set_picker_geometry(Rect::new(0, 0, 40, 12), 0);
    // Click on row 2 (second list row) — the default selection is row 0,
    // so this is a focus-only click; picker stays open with new selection.
    h.click(5, 2);
    assert!(
        h.picker_open(),
        "first click on a non-selected row should only focus"
    );
    assert_eq!(h.editor.picker_selected_for_test(), 1);
    // Clicking the same row again activates and closes the picker.
    h.click(5, 2);
    assert!(
        !h.picker_open(),
        "second click on the focused row should activate"
    );
}

#[test]
fn click_on_already_selected_row_activates_immediately() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.set_picker_geometry(Rect::new(0, 0, 40, 12), 0);
    // Row 1 is the first list row, which matches the default selection (0).
    h.click(5, 1);
    assert!(
        !h.picker_open(),
        "click on the currently-selected row should activate"
    );
}

#[test]
fn click_outside_picker_is_ignored() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.set_picker_geometry(Rect::new(10, 5, 40, 12), 0);
    // Click well above the overlay.
    h.click(0, 0);
    assert!(
        h.picker_open(),
        "click outside overlay should not close picker"
    );
}

#[test]
fn click_on_header_row_is_ignored() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.set_picker_geometry(Rect::new(0, 0, 40, 12), 0);
    h.click(5, 0); // header row
    assert!(h.picker_open(), "click on header row should not activate");
}

#[test]
fn ex_grep_command_prefills_query() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.cmd("Grep hello");
    assert_eq!(h.picker_kind(), Some("grep"));
    assert_eq!(h.picker_query(), Some("hello"));
}
