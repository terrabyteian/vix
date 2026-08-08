//! Ex commands: :w, :q, :wq, :x, :q!, :e, :noh, :ls, :b, :bd, :%s flags.

#![allow(non_snake_case)]

use std::fs;
use vix_tui::testing::Harness;

#[test]
fn quit_on_clean_buffer_sets_quit_flag() {
    let mut h = Harness::with_text("hello\n");
    h.cmd("q");
    // Empty buffer + no other buffers → close_buffer sets quit.
    assert!(h.quit_requested());
}

#[test]
fn quit_on_dirty_buffer_blocks() {
    let mut h = Harness::with_text("hello\n");
    h.keys("dw");
    h.cmd("q");
    assert!(!h.quit_requested());
    assert!(h.msg().contains("E37"));
}

#[test]
fn force_quit_dirty_buffer() {
    let mut h = Harness::with_text("hello\n");
    h.keys("dw");
    h.cmd("q!");
    assert!(h.quit_requested());
}

#[test]
fn triple_esc_force_quits_dirty_buffer() {
    let mut h = Harness::with_text("hello\n");
    h.keys("dw");
    h.keys("<Esc><Esc><Esc>");
    assert!(h.quit_requested());
}

#[test]
fn double_esc_does_not_quit() {
    let mut h = Harness::with_text("hello\n");
    h.keys("dw");
    h.keys("<Esc><Esc>");
    assert!(!h.quit_requested());
}

#[test]
fn esc_streak_interrupted_by_another_key_does_not_quit() {
    let mut h = Harness::with_text("hello\n");
    h.keys("dw");
    h.keys("<Esc>x<Esc><Esc>");
    assert!(!h.quit_requested());
}

#[test]
fn triple_esc_from_insert_mode_quits() {
    let mut h = Harness::with_text("hello\n");
    h.keys("i");
    h.keys("<Esc><Esc><Esc>");
    assert!(h.quit_requested());
}

#[test]
fn double_esc_shows_quit_warning_message() {
    let mut h = Harness::with_text("hello\n");
    h.keys("dw");
    h.keys("<Esc><Esc>");
    assert!(h.msg().contains("Press Esc again"));
}

#[test]
fn write_persists_changes_to_disk() {
    let dir = tempdir();
    let path = dir.join("write_test.txt");
    fs::write(&path, "old\n").unwrap();
    let mut h = Harness::with_text_and_path("new content\n", path.clone());
    // Buffer was synthesized but path is set; mark dirty by editing.
    h.keys("oappended<Esc>");
    h.cmd("w");
    let on_disk = fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("appended"));
    assert!(!h.dirty());
}

#[test]
fn write_then_quit() {
    let dir = tempdir();
    let path = dir.join("wq_test.txt");
    fs::write(&path, "old\n").unwrap();
    let mut h = Harness::with_text_and_path("hello\n", path.clone());
    h.keys("Aworld<Esc>");
    h.cmd("wq");
    assert!(h.quit_requested());
    let on_disk = fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("helloworld"));
}

#[test]
fn noh_clears_search_highlight() {
    let mut h = Harness::with_text("foo bar foo\n");
    h.keys("/foo<CR>");
    // hl_search is on now — :noh turns it off (we can't see it from outside,
    // but we can verify the command runs without setting an error msg).
    h.cmd("noh");
    assert_eq!(h.msg(), "");
}

#[test]
fn edit_loads_a_new_path() {
    let dir = tempdir();
    let path = dir.join("loaded.txt");
    fs::write(&path, "from disk\n").unwrap();
    let mut h = Harness::with_text("starting\n");
    h.cmd(&format!("e {}", path.display()));
    h.assert_text("from disk\n");
    // Original buffer is parked.
    assert_eq!(h.parked_count(), 1);
}

#[test]
fn edit_reloads_current_buffer_from_disk() {
    let dir = tempdir();
    let path = dir.join("reload.txt");
    fs::write(&path, "v1\n").unwrap();
    let mut h = Harness::with_text_and_path("v1\n", path.clone());
    fs::write(&path, "v2\n").unwrap();
    h.cmd("e");
    h.assert_text("v2\n");
    assert!(!h.dirty());
}

#[test]
fn edit_refuses_to_reload_dirty_buffer() {
    let dir = tempdir();
    let path = dir.join("dirty.txt");
    fs::write(&path, "v1\n").unwrap();
    let mut h = Harness::with_text_and_path("v1\n", path.clone());
    h.keys("Adirty<Esc>");
    fs::write(&path, "v2\n").unwrap();
    h.cmd("e");
    // Dirty + no force → buffer untouched, error reported.
    assert!(h.text().contains("dirty"));
    assert!(h.msg().contains("E37"));
    assert!(h.dirty());
}

#[test]
fn edit_force_reloads_dirty_buffer() {
    let dir = tempdir();
    let path = dir.join("force_reload.txt");
    fs::write(&path, "v1\n").unwrap();
    let mut h = Harness::with_text_and_path("v1\n", path.clone());
    h.keys("Adirty<Esc>");
    fs::write(&path, "v2\n").unwrap();
    h.cmd("e!");
    h.assert_text("v2\n");
    assert!(!h.dirty());
}

#[test]
fn edit_without_path_reports_error() {
    let mut h = Harness::with_text("scratch\n");
    h.cmd("e");
    assert!(h.msg().contains("E32"));
}

#[test]
fn substitute_no_pattern_yields_error() {
    let mut h = Harness::with_text("foo\n");
    h.cmd("%s");
    assert!(h.msg().contains("E471") || h.msg().contains("usage"));
}

#[test]
fn unknown_command_reports_not_implemented() {
    let mut h = Harness::with_text("foo\n");
    h.cmd("nosuchcommand");
    assert!(h.msg().starts_with("not implemented"));
}

#[test]
fn buffer_picker_opens() {
    let mut h = Harness::with_text("a\n");
    h.cmd("ls");
    assert!(h.picker_open());
}

#[test]
fn empty_command_is_noop() {
    let mut h = Harness::with_text("hello\n");
    let before = h.text();
    h.cmd("");
    assert_eq!(h.text(), before);
    assert_eq!(h.msg(), "");
}

#[test]
fn wq_blocks_when_no_path() {
    let mut h = Harness::with_text("anonymous\n");
    h.keys("Adirty<Esc>");
    h.cmd("wq");
    // No path → save fails → quit shouldn't happen.
    assert!(!h.quit_requested());
    assert!(h.dirty());
}

/// A scratch directory, unique per call. The timestamp keeps runs apart
/// (these dirs leak in /tmp and pids get reused, so a stale one could
/// otherwise be adopted by a later run); the atomic counter keeps *calls*
/// apart, since two tests can enter this function in the same wall-clock
/// nanosecond and would then race on one shared directory.
fn tempdir() -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vix-test-{}-{}-{n}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}
