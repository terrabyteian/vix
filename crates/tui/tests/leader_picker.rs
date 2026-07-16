//! `<Space>` leader / `<C-p>` open the unified "omni" picker: one query
//! live-searches file names AND file contents, blended into one ranked list.
//! `<Tab>` cycles the source filter All → Files → Content → All, preserving
//! the query and marks. The picker is single-mode, fzf-style: it is always
//! typing. Printable keys (no Ctrl/Alt) filter the query; navigation, marks,
//! and picker-management all live on Ctrl/Alt chords or non-printable keys
//! (arrows, Home/End, PageUp/PageDown) so they never collide with query
//! characters. `<Enter>` opens the highlighted row (or every marked row, if
//! any are marked); `<Esc>`/`<C-c>` closes immediately regardless of query.

#![allow(non_snake_case)]

use ratatui::layout::Rect;
use std::fs;
use vix_tui::testing::Harness;

/// Process-wide lock serializing these tests. The current directory is
/// global process state, and every test `chdir`s into its own fixture then
/// spawns a streaming file scan that captures the cwd — so two tests running
/// in parallel would scan each other's directories. Each `setup_*` returns
/// the held guard (bound as `_dir`), which keeps the lock for the test's
/// whole body.
static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A held `CWD_LOCK` guard. Kept alive by the test's `let _dir = …` binding
/// so the next test can't chdir until this one returns.
type CwdGuard = std::sync::MutexGuard<'static, ()>;

fn lock_cwd() -> CwdGuard {
    // Recover from a poisoned lock: a panicking test still leaves the cwd
    // invariant we care about (we always chdir before doing anything).
    CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Build a temp dir with a few files and chdir into it under the cwd lock.
/// (Tempdirs leak in /tmp; only the returned guard matters to the caller.)
fn setup_repo() -> CwdGuard {
    let guard = lock_cwd();
    let dir = tempdir();
    fs::write(dir.join("alpha.txt"), "hello world\nthe quick brown fox\n").unwrap();
    fs::write(dir.join("beta.txt"), "lorem ipsum dolor\n").unwrap();
    fs::write(dir.join("gamma.rs"), "fn main() { println!(\"hello\"); }\n").unwrap();
    std::env::set_current_dir(&dir).unwrap();
    guard
}

/// A repo where one file is NAMED `hello.txt` and another (`world.rs`)
/// CONTAINS the word `hello`. Lets the blend tests assert a file-name row
/// ranks above a content hit for the same query.
fn setup_blend_repo() -> CwdGuard {
    let guard = lock_cwd();
    let dir = tempdir();
    fs::write(dir.join("hello.txt"), "unrelated contents\n").unwrap();
    fs::write(dir.join("world.rs"), "fn hello() { /* hi */ }\n").unwrap();
    std::env::set_current_dir(&dir).unwrap();
    guard
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
fn space_f_opens_omni_picker_all_filter() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    assert!(!h.picker_open());
    h.keys("<Space>f");
    h.pump_picker();
    assert!(h.picker_open());
    assert_eq!(h.picker_kind(), Some("omni"));
    assert_eq!(h.editor.picker_filter_for_test(), Some("all"));
    assert_eq!(h.picker_query(), Some(""));
}

#[test]
fn space_g_opens_omni_content_filter() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>g");
    h.pump_picker();
    assert!(h.picker_open());
    assert_eq!(h.picker_kind(), Some("omni"));
    assert_eq!(h.editor.picker_filter_for_test(), Some("content"));
}

#[test]
fn ctrl_p_opens_omni_picker() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    assert!(!h.picker_open());
    h.keys("<C-p>");
    h.pump_picker();
    assert!(h.picker_open());
    assert_eq!(h.picker_kind(), Some("omni"));
    assert_eq!(h.editor.picker_filter_for_test(), Some("all"));
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
fn tab_cycles_filter_preserving_query() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>fhe");
    assert_eq!(h.editor.picker_filter_for_test(), Some("all"));
    assert_eq!(h.picker_query(), Some("he"));
    // All -> Files -> Content -> All, query carries over the whole way.
    h.keys("<Tab>");
    assert_eq!(h.editor.picker_filter_for_test(), Some("files"));
    assert_eq!(h.picker_query(), Some("he"));
    h.keys("<Tab>");
    assert_eq!(h.editor.picker_filter_for_test(), Some("content"));
    assert_eq!(h.picker_query(), Some("he"));
    h.keys("<Tab>");
    assert_eq!(h.editor.picker_filter_for_test(), Some("all"));
    assert_eq!(h.picker_query(), Some("he"));
}

#[test]
fn omni_blends_names_first_then_contents() {
    let _dir = setup_blend_repo();
    let mut h = Harness::with_text("scratch\n");
    h.keys("<Space>f");
    h.keys("hello");
    h.flush_picker();
    // Both sources contributed: a file NAMED hello.txt and a content hit.
    let (files, content) = h.editor.picker_counts_for_test();
    assert!(
        files >= 1,
        "expected a file-name hit for hello.txt: {files}"
    );
    assert!(
        content >= 1,
        "expected a content hit for `hello`: {content}"
    );
    // The file-name row ranks first (bands: file hits outrank content hits).
    assert_eq!(h.editor.picker_selected_source_for_test(), Some("file"));
}

#[test]
fn files_filter_hides_content_rows() {
    let _dir = setup_blend_repo();
    let mut h = Harness::with_text("scratch\n");
    h.keys("<Space>f"); // All
    h.keys("hello");
    h.flush_picker();
    let with_content = h.editor.picker_matches_count_for_test();
    // Switch to Files: content rows drop, so the match count shrinks (or at
    // least never grows) and the top row is still a file.
    h.keys("<Tab>");
    assert_eq!(h.editor.picker_filter_for_test(), Some("files"));
    h.flush_picker();
    let files_only = h.editor.picker_matches_count_for_test();
    assert!(
        files_only <= with_content,
        "files filter must not add rows: {files_only} vs {with_content}"
    );
    assert_eq!(h.editor.picker_selected_source_for_test(), Some("file"));
}

#[test]
fn content_filter_hides_file_rows() {
    let _dir = setup_blend_repo();
    let mut h = Harness::with_text("scratch\n");
    h.keys("<Space>g"); // Content
    h.keys("hello");
    h.flush_picker();
    assert_eq!(h.editor.picker_filter_for_test(), Some("content"));
    // Every visible row is a content hit — the selected source is grep.
    assert!(h.editor.picker_matches_count_for_test() >= 1);
    assert_eq!(h.editor.picker_selected_source_for_test(), Some("grep"));
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
    // Exactly one file name matches; `gamma` is short enough that no content
    // hit is expected in this fixture.
    let (files, _content) = h.editor.picker_counts_for_test();
    assert_eq!(files, 1);
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
    let (files, _content) = h.editor.picker_counts_for_test();
    assert_eq!(files, 3);
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
fn ex_files_command_opens_omni_picker() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.cmd("Files");
    assert_eq!(h.picker_kind(), Some("omni"));
    assert_eq!(h.editor.picker_filter_for_test(), Some("all"));
    h.keys("<Tab>");
    assert_eq!(h.editor.picker_filter_for_test(), Some("files"));
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
    // Compact-box list rect (Omni renders through the compact centered box).
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
fn ex_grep_command_prefills_query() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.cmd("Grep hello");
    assert_eq!(h.picker_kind(), Some("omni"));
    assert_eq!(h.editor.picker_filter_for_test(), Some("content"));
    assert_eq!(h.picker_query(), Some("hello"));
}

#[test]
fn content_short_query_has_no_matches() {
    // The omni content source requires 2+ chars before it walks; below that,
    // a Content-filtered query has no content rows (the hint is render-only,
    // so we assert the model state: zero content matches).
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>g");
    h.pump_picker();
    assert!(h.picker_open());
    assert_eq!(h.editor.picker_filter_for_test(), Some("content"));
    assert_eq!(h.picker_query(), Some(""));
    assert_eq!(h.editor.picker_matches_count_for_test(), 0);
    h.keys("h");
    h.flush_picker();
    assert_eq!(h.picker_query(), Some("h"));
    let (_files, content) = h.editor.picker_counts_for_test();
    assert_eq!(content, 0, "1-char content query must not walk");
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
fn marks_survive_a_full_filter_cycle() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    // Mark two file rows.
    h.keys("<Nul><Nul>");
    assert_eq!(h.editor.picker_marked_count_for_test(), 2);
    // Cycle All -> Files -> Content -> All. Marks are ItemRef-keyed, so the
    // file marks survive the whole cycle.
    h.keys("<Tab><Tab><Tab>");
    assert_eq!(h.editor.picker_filter_for_test(), Some("all"));
    assert_eq!(h.editor.picker_marked_count_for_test(), 2);
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
fn empty_query_omnibox_shows_last_opened_file_on_top() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("scratch\n");
    // Point the harness at a real (temp) recent-files store — the default
    // harness disables persistence, so this opts back in deliberately.
    h.set_recent_data_file(tempdir().join("recent-store"));

    // Open beta.txt so it's recorded as this project's most-recently-opened
    // file.
    h.cmd("e beta.txt");
    assert_eq!(
        h.editor
            .buffer
            .path()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned()),
        Some("beta.txt".to_string()),
        "beta.txt should be the active buffer after :e"
    );

    // Close (implicitly, by switching to a fresh scratch buffer state isn't
    // needed — the omnibox is opened fresh each time) and reopen the
    // omnibox: the empty-query, All-filter view must show the just-opened
    // file on top instead of the shortest-path fallback.
    h.keys("<Space>f");
    h.pump_picker();
    assert!(h.picker_open());
    assert_eq!(h.editor.picker_filter_for_test(), Some("all"));
    assert_eq!(h.picker_query(), Some(""));
    assert_eq!(
        h.editor.picker_top_display_for_test(),
        Some("beta.txt".to_string()),
        "the recents view should surface the last-opened file on top"
    );
    let (files, _content) = h.editor.picker_counts_for_test();
    assert_eq!(files, 3, "footer's file count stays honest under the view");

    // Enter on the top row reopens beta.txt.
    h.keys("<CR>");
    assert!(!h.picker_open(), "enter should pick the recents row");
    h.assert_text("lorem ipsum dolor\n");
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

// --- "Esc at launch quits vix" -------------------------------------------
//
// `vix` with no file (or a directory) argument boots into the omnibox with
// an empty `[No Name]` placeholder buffer behind it. Dismissing that launch
// omnibox without picking anything leaves nothing to edit, so it quits the
// whole process instead of dropping the user into an empty buffer. Picking
// a result — by Enter, a marked multi-select, or a mouse click — cancels
// that and behaves exactly like any other omnibox pick. Opening the
// omnibox again afterwards (or from any other in-editor entry point) is
// unaffected: only the launch omnibox itself carries the quit-on-close
// behavior.

#[test]
fn launch_esc_quits() {
    let _dir = setup_repo();
    let mut h = Harness::new();
    h.open_launch_picker();
    h.pump_picker();
    assert!(h.picker_open());
    h.keys("<Esc>");
    assert!(!h.picker_open());
    assert!(h.quit_requested(), "Esc at launch should quit vix");
}

#[test]
fn launch_ctrl_c_quits() {
    let _dir = setup_repo();
    let mut h = Harness::new();
    h.open_launch_picker();
    h.pump_picker();
    assert!(h.picker_open());
    h.keys("<C-c>");
    assert!(!h.picker_open());
    assert!(h.quit_requested(), "Ctrl-C at launch should quit vix");
}

#[test]
fn launch_select_does_not_quit() {
    let _dir = setup_repo();
    let mut h = Harness::new();
    h.open_launch_picker();
    h.pump_picker();
    h.keys("alpha");
    h.pump_picker();
    h.keys("<CR>");
    assert!(!h.picker_open());
    assert!(
        !h.quit_requested(),
        "picking a file at launch should not quit"
    );
    h.assert_text("hello world\nthe quick brown fox\n");

    // The launch flag must not linger: opening the omnibox again in-editor
    // and Esc-ing out of it just closes the picker as normal.
    h.keys("<Space>f");
    h.pump_picker();
    assert!(h.picker_open());
    h.keys("<Esc>");
    assert!(!h.picker_open());
    assert!(
        !h.quit_requested(),
        "a later in-editor Esc must not quit — the launch flag shouldn't linger"
    );
}

#[test]
fn in_editor_esc_does_not_quit() {
    let _dir = setup_repo();
    let mut h = Harness::with_text("hello\n");
    h.keys("<Space>f");
    h.pump_picker();
    assert!(h.picker_open());
    h.keys("<Esc>");
    assert!(!h.picker_open());
    assert!(
        !h.quit_requested(),
        "Esc on an in-editor omnibox must never quit"
    );
}
