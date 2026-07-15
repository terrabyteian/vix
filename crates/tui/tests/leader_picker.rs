//! `<Space>` leader opens the unified Files/Grep picker. `<Tab>` toggles
//! submode preserving the query. The picker is single-mode, fzf-style: it is
//! always typing. Printable keys (no Ctrl/Alt) filter the query; navigation,
//! marks, and picker-management all live on Ctrl/Alt chords or non-printable
//! keys (arrows, Home/End, PageUp/PageDown) so they never collide with query
//! characters. `<Enter>` opens the highlighted row (or every marked row, if
//! any are marked); `<Esc>`/`<C-c>` closes immediately regardless of query.

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
    h.pump_picker();
    assert!(h.picker_open());
    assert_eq!(h.picker_kind(), Some("files"));
    assert_eq!(h.picker_query(), Some(""));
}

#[test]
fn space_g_opens_grep_picker() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>g");
    h.pump_picker();
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
fn esc_closes_immediately() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    h.keys("abc");
    assert_eq!(h.picker_query(), Some("abc"));
    // A single Esc closes the picker outright, regardless of query — there
    // is no intermediate "clear query first" step.
    h.keys("<Esc>");
    assert!(!h.picker_open());
}

#[test]
fn ctrl_c_closes_immediately() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    assert!(h.picker_open());
    h.keys("<C-c>");
    assert!(!h.picker_open());
}

#[test]
fn enter_opens_selected_row() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    assert!(h.picker_open());
    h.keys("<CR>");
    assert!(!h.picker_open(), "enter should open the selected row");
}

#[test]
fn ctrl_jk_and_arrows_move_selection() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    let before = h.editor.picker_selected_for_test();
    h.keys("<C-j>");
    assert_eq!(h.editor.picker_selected_for_test(), before + 1);
    h.keys("<C-k>");
    assert_eq!(h.editor.picker_selected_for_test(), before);
    h.keys("<Down>");
    assert_eq!(h.editor.picker_selected_for_test(), before + 1);
    h.keys("<Up>");
    assert_eq!(h.editor.picker_selected_for_test(), before);
}

#[test]
fn typing_filters_matches() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    let before = h.editor.picker_matches_count_for_test();
    assert_eq!(before, 3, "alpha.txt, beta.txt, gamma.rs");
    h.keys("gamma");
    h.flush_picker();
    assert_eq!(h.picker_query(), Some("gamma"));
    assert_eq!(h.editor.picker_matches_count_for_test(), 1);
}

#[test]
fn ctrl_u_clears_query() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>fgamma");
    assert_eq!(h.picker_query(), Some("gamma"));
    h.keys("<C-u>");
    assert_eq!(h.picker_query(), Some(""));
    h.flush_picker();
    assert_eq!(h.editor.picker_matches_count_for_test(), 3);
}

#[test]
fn ctrl_w_deletes_trailing_word() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    h.keys("foo bar");
    assert_eq!(h.picker_query(), Some("foo bar"));
    h.keys("<C-w>");
    assert_eq!(h.picker_query(), Some("foo "));
    h.keys("<C-w>");
    assert_eq!(h.picker_query(), Some(""));
}

#[test]
fn pagedown_moves_selection_forward_and_pageup_back() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    assert_eq!(h.editor.picker_selected_for_test(), 0);
    h.keys("<PageDown>");
    let after = h.editor.picker_selected_for_test();
    assert!(after > 0, "PageDown should move selection down");
    h.keys("<PageUp>");
    assert_eq!(h.editor.picker_selected_for_test(), 0);
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
    h.pump_picker();
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
    h.pump_picker();
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
    h.pump_picker();
    assert!(h.picker_open());
    // Fullscreen picker geometry: list pane only (no header inside the rect).
    h.set_picker_geometry(Rect::new(0, 3, 40, 9), 0);
    // Click on the second list row — default selection is row 0, so this is
    // a focus-only click; picker stays open with new selection.
    h.click(5, 4);
    assert!(
        h.picker_open(),
        "first click on a non-selected row should only focus"
    );
    assert_eq!(h.editor.picker_selected_for_test(), 1);
    // Clicking the same row again activates and closes the picker.
    h.click(5, 4);
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
    h.pump_picker();
    h.set_picker_geometry(Rect::new(0, 3, 40, 9), 0);
    // Row 3 is the first list row in the rect, matching default selection 0.
    h.click(5, 3);
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
    h.pump_picker();
    h.set_picker_geometry(Rect::new(10, 5, 40, 12), 0);
    // Click well above the overlay.
    h.click(0, 0);
    assert!(
        h.picker_open(),
        "click outside overlay should not close picker"
    );
}

#[test]
fn click_above_list_area_is_ignored() {
    // The fullscreen picker reserves rows 0..2 for chrome (tabs / prompt /
    // separator). `last_picker_rect` is the list area, so clicks above it
    // fall outside the rect and are ignored — same behavior the old
    // overlay enforced via a "row 0 is header" check.
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    h.set_picker_geometry(Rect::new(0, 3, 40, 9), 0);
    h.click(5, 0); // chrome row, above the list
    assert!(h.picker_open(), "click above the list should not activate");
}

#[test]
fn ex_grep_command_prefills_query() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.cmd("Grep hello");
    assert_eq!(h.picker_kind(), Some("grep"));
    assert_eq!(h.picker_query(), Some("hello"));
}

#[test]
fn grep_short_query_shows_hint() {
    // Grep requires 2+ chars before it populates items; below that, the
    // match list is empty (the hint itself is render-only, so we just
    // assert there's no crash and no matches).
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>g");
    h.pump_picker();
    assert!(h.picker_open());
    assert_eq!(h.picker_query(), Some(""));
    assert_eq!(h.editor.picker_matches_count_for_test(), 0);
    h.keys("h");
    h.flush_picker();
    assert_eq!(h.picker_query(), Some("h"));
    assert_eq!(h.editor.picker_matches_count_for_test(), 0);
}

#[test]
fn end_jumps_to_last_match() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    assert_eq!(h.editor.picker_selected_for_test(), 0);
    h.keys("<End>");
    let last = h.editor.picker_selected_for_test();
    assert!(last > 0, "End should jump past the first row");
    // Pressing End again is idempotent.
    h.keys("<End>");
    assert_eq!(h.editor.picker_selected_for_test(), last);
}

#[test]
fn home_jumps_to_first_match() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    h.keys("<End>");
    assert!(h.editor.picker_selected_for_test() > 0);
    h.keys("<Home>");
    assert_eq!(h.editor.picker_selected_for_test(), 0);
}

#[test]
fn ctrl_space_marks_and_advances_alt_c_clears() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    assert_eq!(h.editor.picker_marked_count_for_test(), 0);
    assert_eq!(h.editor.picker_selected_for_test(), 0);
    // `<Nul>` (Ctrl-Space) marks the current row and auto-advances.
    h.keys("<Nul>");
    assert_eq!(h.editor.picker_marked_count_for_test(), 1);
    assert_eq!(
        h.editor.picker_selected_for_test(),
        1,
        "mark should auto-advance selection"
    );
    h.keys("<Nul>");
    assert_eq!(h.editor.picker_marked_count_for_test(), 2);
    assert_eq!(h.editor.picker_selected_for_test(), 2);
    // Move back and re-mark the same row — toggles it off (still advances).
    h.keys("<Up>");
    h.keys("<Nul>");
    assert_eq!(h.editor.picker_marked_count_for_test(), 1);
    // `<A-c>` clears all marks.
    h.keys("<A-c>");
    assert_eq!(h.editor.picker_marked_count_for_test(), 0);
}

#[test]
fn enter_with_marked_opens_all_marked_files() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    // Each `<Nul>` marks the current row and auto-advances, so three in a
    // row marks rows 0, 1, 2.
    h.keys("<Nul><Nul><Nul>");
    assert_eq!(h.editor.picker_marked_count_for_test(), 3);
    h.keys("<CR>");
    assert!(!h.picker_open(), "picker should close on batch open");
    // The starting "hello" scratch buffer parks alongside the 3 opened
    // files (1 active + 3 parked). Tab/Ctrl-O navigates between them.
    assert_eq!(h.buffer_count(), 4);
}

#[test]
fn tab_clears_marks_when_swapping_files_and_grep() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    h.keys("<Nul><Nul>");
    assert_eq!(h.editor.picker_marked_count_for_test(), 2);
    // Tab switches to grep; marks would point at the wrong items, so they
    // must be dropped.
    h.keys("<Tab>");
    assert_eq!(h.editor.picker_marked_count_for_test(), 0);
}

#[test]
fn space_types_into_query() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    // Plain space has no modifier, so it's a printable char like any other:
    // it appends to the query rather than marking a row.
    h.keys("<Space>f");
    h.pump_picker();
    h.keys("a  ");
    assert_eq!(h.picker_query(), Some("a  "));
    assert_eq!(h.editor.picker_marked_count_for_test(), 0);
}

#[test]
fn picker_opens_instantly_and_streams_items_in() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    // The picker is open and interactive before any scan results exist.
    h.keys("<Space>f");
    assert!(h.picker_open());
    // Draining the streaming scan fills the list.
    h.pump_picker();
    assert_eq!(h.editor.picker_matches_count_for_test(), 3);
}

#[test]
fn grep_enter_acts_on_whats_on_screen() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.cmd("Grep hello");
    h.pump_picker();
    let settled = h.editor.picker_matches_count_for_test();
    assert!(settled > 0, "expected hits for 'hello'");
    // Type more query characters but press Enter before the debounce
    // flushes: Enter must act on the visible (old-query) matches instead of
    // blocking on a fresh walk.
    h.keys("xyzzy");
    h.keys("<CR>");
    assert!(!h.picker_open(), "enter opens the on-screen selection");
    assert!(
        h.editor.buffer.path().is_some(),
        "a grep hit buffer should be active"
    );
}

#[test]
fn closing_picker_cancels_streaming_sources() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    // Close immediately — the scan worker is likely still running.
    h.keys("<Esc>");
    assert!(!h.picker_open());
    // Draining after close must be a no-op (sources were cancelled), and
    // most importantly must not panic or resurrect picker state.
    h.pump_picker();
    assert!(!h.picker_open());
}
