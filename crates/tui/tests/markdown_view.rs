//! Rendered-markdown view: default view mode by path/size, `Space m` /
//! `:preview` / `:pv` / `:raw` toggling, rendered-view key routing (scroll
//! vs. edit-drops-to-raw), per-buffer persistence across Tab cycling, and
//! `:help` opening rendered.

#![allow(non_snake_case)]

use ratatui::layout::Rect;
use std::fs;
use std::path::PathBuf;
use vix_core::Mode;
use vix_tui::testing::Harness;
use vix_tui::ViewMode;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // workspace
    p.push("tests/fixtures");
    p.push(name);
    p
}

fn rich_md() -> String {
    fs::read_to_string(fixture("rich.md")).unwrap()
}

/// Write `content` to a fresh temp file with the given extension and return
/// its path. Mirrors the pattern in `multi_buffer.rs`'s `tmpfile` helper.
fn tmpfile(content: &str, ext: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("vix-mdview-{}-{n}.{ext}", std::process::id()));
    fs::write(&p, content).unwrap();
    p
}

// ---------------------------------------------------------------------
// Default view mode
// ---------------------------------------------------------------------

#[test]
fn md_path_defaults_to_rendered() {
    let h = Harness::with_text_and_path(&rich_md(), fixture("rich.md"));
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
}

#[test]
fn rs_path_defaults_to_raw() {
    let h = Harness::with_text_and_path("fn main() {}\n", fixture("sample.rs"));
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
}

#[test]
fn pathless_buffer_defaults_to_raw() {
    let h = Harness::with_text("no path here\n");
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
}

#[test]
fn oversized_markdown_defaults_to_raw() {
    // Cheap way to build a >2MB buffer: one long repeated line, no newlines.
    // A plain byte-length check, not layout, so this stays fast (well under
    // a second — no fence/heading structure to parse for the default-mode
    // check itself).
    let big = "x".repeat(2 * 1024 * 1024 + 16);
    let h = Harness::with_text_and_path(&big, "huge.md");
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
}

// ---------------------------------------------------------------------
// Toggling: Space m / :preview / :pv / :raw
// ---------------------------------------------------------------------

#[test]
fn leader_m_toggles_rendered_and_raw() {
    let mut h = Harness::with_text_and_path(&rich_md(), fixture("rich.md"));
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
    h.keys("<Space>m");
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
    h.keys("<Space>m");
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
}

#[test]
fn ex_raw_and_preview_toggle_like_leader_m() {
    let mut h = Harness::with_text_and_path(&rich_md(), fixture("rich.md"));
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
    h.cmd("raw");
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
    h.cmd("preview");
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
    // pv is the short alias.
    h.cmd("raw");
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
    h.cmd("pv");
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
}

#[test]
fn preview_when_already_rendered_leaves_it_rendered() {
    let mut h = Harness::with_text_and_path(&rich_md(), fixture("rich.md"));
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
    h.cmd("preview");
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
    assert!(h.msg().contains("already in the rendered view"));
}

#[test]
fn preview_on_non_markdown_buffer_stays_raw() {
    let mut h = Harness::with_text_and_path("fn main() {}\n", fixture("sample.rs"));
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
    h.cmd("preview");
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
    assert!(
        h.msg().contains("not a markdown buffer"),
        "msg: {}",
        h.msg()
    );
}

// ---------------------------------------------------------------------
// Rendered-view key routing
// ---------------------------------------------------------------------

#[test]
fn insert_key_in_rendered_drops_to_raw_insert_and_types() {
    let mut h = Harness::with_text_and_path("line one\n", "note.md");
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
    h.keys("i");
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
    h.assert_mode(Mode::Insert);
    h.keys("hey ");
    h.assert_text("hey line one\n");
}

#[test]
fn x_in_rendered_drops_to_raw_normal_without_editing() {
    let mut h = Harness::with_text_and_path("line one\n", "note.md");
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
    h.keys("x");
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
    h.assert_mode(Mode::Normal);
    h.assert_text("line one\n");
}

#[test]
fn jk_in_rendered_do_not_move_cursor_or_edit_text() {
    let mut h = Harness::with_text_and_path(&rich_md(), fixture("rich.md"));
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
    let before_text = h.text();
    let before_cursor = h.cursor();
    h.keys("jjjkjk");
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
    assert_eq!(h.cursor(), before_cursor);
    assert_eq!(h.text(), before_text);
}

// ---------------------------------------------------------------------
// Per-buffer view mode across Tab cycling
// ---------------------------------------------------------------------

#[test]
fn view_mode_is_per_buffer_across_tab_cycle() {
    let rs_path = tmpfile("fn main() {}\n", "rs");
    let mut h = Harness::with_text_and_path(&rich_md(), fixture("rich.md"));
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);

    h.cmd(&format!("e {}", rs_path.display()));
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);

    // Tab cycles to the parked .md buffer — should come back Rendered.
    h.keys("<Tab>");
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);

    // Tab again cycles back to the .rs buffer — should still be Raw.
    h.keys("<Tab>");
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
}

// ---------------------------------------------------------------------
// Toggle round-trip: rendered scroll position maps back to a raw cursor line
// ---------------------------------------------------------------------

#[test]
fn switching_back_to_raw_after_scrolling_moves_cursor_past_top() {
    let mut h = Harness::with_text_and_path(&rich_md(), fixture("rich.md"));
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
    // The buffer opens Rendered but the layout is only built lazily
    // (normally by `update()`, which the harness never calls). Round-trip
    // through raw -> rendered once so `toggle_markdown_view` builds it
    // itself, per its doc comment.
    h.cmd("raw");
    h.cmd("preview");
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);

    // Scroll down several display lines into the multi-section document.
    h.keys("jjjjjjjjjjjjjjjjjjjj");

    h.keys("<Space>m");
    assert_eq!(h.editor.view_mode(), ViewMode::Raw);
    let (line, _col) = h.cursor();
    assert!(
        line > 0,
        "expected cursor past line 0 after scroll+toggle, got {line}"
    );
}

// ---------------------------------------------------------------------
// :help opens Rendered
// ---------------------------------------------------------------------

#[test]
fn help_opens_rendered() {
    let mut h = Harness::with_text("hello\n");
    h.cmd("help");
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
}

// ---------------------------------------------------------------------
// Mouse drag-select + copy in the rendered view
// ---------------------------------------------------------------------

/// Build a harness on `rich.md`, already rendered, with a laid-out 80x20
/// content rect at the origin. The layout is built lazily (normally by
/// `update()`, which the harness never calls) and its width comes from
/// `last_content_rect` at toggle time — so geometry has to be set *before*
/// round-tripping raw -> rendered, mirroring the dance in
/// `switching_back_to_raw_after_scrolling_moves_cursor_past_top` above.
fn rendered_with_geometry() -> Harness {
    let mut h = Harness::with_text_and_path(&rich_md(), fixture("rich.md"));
    h.set_render_geometry(Rect::new(0, 0, 80, 20), 0);
    h.cmd("raw");
    h.cmd("preview");
    assert_eq!(h.editor.view_mode(), ViewMode::Rendered);
    h
}

#[test]
fn drag_selects_and_copies_on_release() {
    let mut h = rendered_with_geometry();
    h.click(0, 0);
    h.drag(79, 1);
    h.mouse_up(79, 1);

    assert!(h.editor.md_selection().is_some());
    let (text, linewise) = h.register();
    assert!(!text.is_empty());
    assert!(!linewise);
    assert!(h.msg().starts_with("copied "), "msg: {}", h.msg());
    // The frontmatter block (rendered with a "▌ " bar prefix) is the top of
    // rich.md's document — its first field name should show up in the copy.
    assert!(text.contains("title"), "text: {text:?}");
}

#[test]
fn click_without_drag_copies_nothing() {
    let mut h = rendered_with_geometry();
    h.click(5, 0);
    h.mouse_up(5, 0);

    let (text, _) = h.register();
    assert!(text.is_empty(), "text: {text:?}");
    assert!(!h.msg().starts_with("copied "), "msg: {}", h.msg());
}

#[test]
fn keypress_clears_selection() {
    let mut h = rendered_with_geometry();
    h.click(0, 0);
    h.drag(79, 1);
    assert!(h.editor.md_selection().is_some());

    h.keys("j");
    assert!(h.editor.md_selection().is_none());
}

#[test]
fn click_outside_content_clears_selection() {
    let mut h = rendered_with_geometry();
    h.click(0, 0);
    h.drag(79, 1);
    assert!(h.editor.md_selection().is_some());

    // Row 25 is below the 20-row content rect set up above.
    h.click(0, 25);
    assert!(h.editor.md_selection().is_none());
}

#[test]
fn drag_past_bottom_edge_scrolls() {
    let mut h = rendered_with_geometry();
    assert_eq!(h.editor.md_scroll_for_test(), 0);
    h.click(0, 19);
    for _ in 0..5 {
        h.drag(0, 25);
    }
    assert!(
        h.editor.md_scroll_for_test() > 0,
        "expected auto-scroll past the bottom edge"
    );
}

#[test]
fn mouse_selection_survives_wheel_scroll() {
    let mut h = rendered_with_geometry();
    h.click(0, 0);
    h.drag(79, 1);
    let before = h.editor.md_selection();
    assert!(before.is_some());

    // Selection endpoints are absolute display-line coords, so a wheel
    // scroll (which only moves md_scroll) must not disturb them.
    h.scroll_down();
    assert_eq!(h.editor.md_selection(), before);
}
