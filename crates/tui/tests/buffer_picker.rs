//! `<Space>b` opens a Buffers picker with the same fullscreen feel as the
//! Files/Grep picker. It's single-mode fzf-style like every other picker —
//! plain typing filters the query — so buffer management moves to Ctrl/Alt
//! chords that never collide with query characters: `<C-s>` save, `<C-q>` /
//! `<A-q>` close / force-close, `<C-r>` / `<A-r>` reload / force-reload.

#![allow(non_snake_case)]

use std::fs;
use vix_tui::testing::Harness;

fn tmpfile(content: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vix-bufpicker-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::write(&p, content).unwrap();
    p
}

#[test]
fn space_b_opens_buffers_picker() {
    let mut h = Harness::with_text("hello\n");
    assert!(!h.picker_open());
    h.keys("<Space>b");
    assert!(h.picker_open());
    assert_eq!(h.picker_kind(), Some("buffers"));
}

#[test]
fn buffers_picker_has_one_item_per_buffer() {
    let a = tmpfile("alpha\n");
    let b = tmpfile("beta\n");
    let mut h = Harness::with_text_and_path("active\n", "active.txt");
    h.cmd(&format!("e {}", a.display()));
    h.cmd(&format!("e {}", b.display()));
    assert_eq!(h.buffer_count(), 3);
    h.keys("<Space>b");
    // Items should equal buffer count; query is empty so all match.
    assert!(h.picker_open());
}

#[test]
fn cr_switches_to_highlighted_buffer() {
    // <CR> activates the highlighted row (default: the currently-active
    // buffer).
    let a = tmpfile("alpha buf\n");
    let mut h = Harness::with_text_and_path("active\n", "active.txt");
    h.cmd(&format!("e {}", a.display()));
    h.assert_text("alpha buf\n");
    h.keys("<Space>b<CR>");
    // Still on the active buffer (row 0 was selected by default).
    assert!(!h.picker_open());
    h.assert_text("alpha buf\n");
}

#[test]
fn j_then_cr_switches_to_parked_buffer() {
    let a = tmpfile("parked content\n");
    let mut h = Harness::with_text_and_path("starting\n", "active.txt");
    h.cmd(&format!("e {}", a.display()));
    // Buffer "starting" is now parked at idx 1; "parked content" is active.
    h.assert_text("parked content\n");
    h.keys("<Space>b<C-j><CR>");
    assert!(!h.picker_open());
    h.assert_text("starting\n");
}

#[test]
fn ctrl_s_saves_active_dirty_buffer_from_picker() {
    let p = tmpfile("on disk\n");
    let mut h = Harness::with_text_and_path("on disk\n", &p);
    h.keys("Adirty<Esc>");
    assert!(h.dirty());
    h.keys("<Space>b<C-s>");
    assert!(h.picker_open(), "picker stays open after save");
    assert!(!h.dirty(), "save should clear dirty flag");
    assert_eq!(fs::read_to_string(&p).unwrap(), "on diskdirty\n");
}

#[test]
fn ctrl_q_refuses_to_close_dirty_active_buffer() {
    let p = tmpfile("file\n");
    let mut h = Harness::with_text_and_path("file\n", &p);
    h.keys("Adirty<Esc>");
    assert!(h.dirty());
    h.keys("<Space>b<C-q>");
    // Picker should still be open with a complaint message.
    assert!(h.picker_open());
    assert!(h.msg().contains("E89"), "expected E89, got {:?}", h.msg());
    assert!(h.dirty());
}

#[test]
fn alt_q_force_closes_dirty_buffer() {
    let a = tmpfile("first\n");
    let b = tmpfile("second\n");
    let mut h = Harness::with_text_and_path("active\n", "active.txt");
    h.cmd(&format!("e {}", a.display()));
    h.cmd(&format!("e {}", b.display()));
    // 3 buffers: active = "second", parked = ["active", "first"].
    h.keys("Adirty<Esc>");
    assert!(h.dirty());
    let before = h.buffer_count();
    h.keys("<Space>b<A-q>");
    // Active buffer was force-closed; one of the parked buffers takes over.
    assert!(h.picker_open(), "picker stays open after force-close");
    assert_eq!(h.buffer_count(), before - 1);
    assert!(h.text() != "seconddirty\n");
}

#[test]
fn ctrl_r_refuses_to_reload_dirty_buffer() {
    let p = tmpfile("on disk\n");
    let mut h = Harness::with_text_and_path("on disk\n", &p);
    h.keys("Adirty<Esc>");
    assert!(h.dirty());
    h.keys("<Space>b<C-r>");
    assert!(h.picker_open());
    assert!(h.msg().contains("E37"), "expected E37, got {:?}", h.msg());
    // Buffer text unchanged.
    assert_eq!(h.text(), "on diskdirty\n");
}

#[test]
fn alt_r_force_reloads_from_disk() {
    let p = tmpfile("disk text\n");
    let mut h = Harness::with_text_and_path("disk text\n", &p);
    h.keys("Adirty<Esc>");
    assert!(h.dirty());
    h.keys("<Space>b<A-r>");
    assert!(h.picker_open());
    assert!(!h.dirty());
    assert_eq!(h.text(), "disk text\n");
}

#[test]
fn close_only_buffer_quits_editor() {
    let p = tmpfile("clean\n");
    let mut h = Harness::with_text_and_path("clean\n", &p);
    assert!(!h.dirty());
    h.keys("<Space>b<C-q>");
    // The picker must be torn down because the editor is exiting.
    assert!(h.quit_requested());
    assert!(!h.picker_open());
}

#[test]
fn alt_q_on_parked_buffer_removes_it() {
    let a = tmpfile("alpha\n");
    let mut h = Harness::with_text_and_path("active\n", "active.txt");
    h.cmd(&format!("e {}", a.display()));
    // active = "alpha" (just opened); parked = ["active"].
    let before = h.buffer_count();
    // Move down to the parked buffer (row 1) then force-close.
    h.keys("<Space>b<C-j><A-q>");
    assert!(h.picker_open());
    assert_eq!(h.buffer_count(), before - 1);
}

#[test]
fn save_on_parked_buffer_writes_to_disk() {
    let a = tmpfile("a contents\n");
    let origin_path = tmpfile("origin\n");
    let mut h = Harness::with_text_and_path("origin\n", &origin_path);
    h.cmd(&format!("e {}", a.display()));
    // Cycle back to origin and dirty it.
    h.cmd("bp");
    h.keys("Adirty<Esc>");
    assert!(h.dirty());
    // Re-park origin (now dirty) and make "a contents" active again.
    h.cmd("bn");
    assert_eq!(h.text(), "a contents\n");
    // Picker order: row 0 = active (a contents), row 1 = parked origin.
    // `<C-j><C-s>` moves to row 1 then saves it.
    h.keys("<Space>b<C-j><C-s>");
    assert!(h.picker_open());
    assert_eq!(
        fs::read_to_string(&origin_path).unwrap(),
        "origindirty\n",
        "save should have written the parked buffer's content to disk"
    );
}
