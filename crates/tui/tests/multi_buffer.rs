//! Multi-buffer flow: :e to open, parking, :bn / :bp / :b N, :bd, hidden
//! semantics (parked dirty buffers persist).

#![allow(non_snake_case)]

use std::fs;
use vix_tui::testing::Harness;

fn tmpfile(content: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vix-multi-{}-{}.txt",
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
fn edit_parks_active_buffer() {
    let p = tmpfile("file two\n");
    let mut h = Harness::with_text("file one\n");
    h.cmd(&format!("e {}", p.display()));
    assert_eq!(h.parked_count(), 1);
    h.assert_text("file two\n");
}

#[test]
fn bn_cycles_to_next_buffer() {
    let a = tmpfile("buffer A\n");
    let b = tmpfile("buffer B\n");
    let mut h = Harness::with_text_and_path("active\n", "irrelevant.txt");
    h.cmd(&format!("e {}", a.display()));
    h.cmd(&format!("e {}", b.display()));
    h.assert_text("buffer B\n");
    h.cmd("bn");
    // bn cycles to the next parked buffer.
    assert!(h.text() != "buffer B\n");
}

#[test]
fn bp_cycles_to_previous_buffer() {
    let a = tmpfile("alpha\n");
    let b = tmpfile("beta\n");
    let mut h = Harness::with_text("starting\n");
    h.cmd(&format!("e {}", a.display()));
    h.cmd(&format!("e {}", b.display()));
    h.assert_text("beta\n");
    // bp from beta: park beta, promote the most-recently-parked = alpha.
    h.cmd("bp");
    h.assert_text("alpha\n");
}

#[test]
fn dirty_buffer_parks_when_switching() {
    let p = tmpfile("on disk\n");
    let mut h = Harness::with_text_and_path("starting\n", "first.txt");
    h.keys("Adirty<Esc>");
    assert!(h.dirty());
    h.cmd(&format!("e {}", p.display()));
    h.assert_text("on disk\n");
    // Cycling back must restore the dirty state.
    h.cmd("bp");
    assert!(h.dirty(), "parked dirty buffer should still be dirty after returning");
    h.assert_text("startingdirty\n");
}

#[test]
fn bd_closes_active_buffer() {
    let a = tmpfile("alpha\n");
    let b = tmpfile("beta\n");
    let mut h = Harness::with_text_and_path("third\n", "third.txt");
    h.cmd(&format!("e {}", a.display()));
    h.cmd(&format!("e {}", b.display()));
    assert_eq!(h.buffer_count(), 3);
    h.cmd("bd");
    assert_eq!(h.buffer_count(), 2);
}

#[test]
fn closing_last_buffer_quits() {
    let mut h = Harness::with_text("hello\n");
    h.cmd("bd");
    assert!(h.quit_requested());
}

#[test]
fn bd_blocks_on_dirty_buffer() {
    let mut h = Harness::with_text("hello\n");
    h.keys("Adirty<Esc>");
    h.cmd("bd");
    assert!(!h.quit_requested());
    assert!(h.msg().contains("E37") || h.msg().contains("write"));
}

#[test]
fn b_numeric_switches_by_index() {
    let a = tmpfile("alpha\n");
    let b = tmpfile("beta\n");
    let mut h = Harness::with_text("starter\n");
    h.cmd(&format!("e {}", a.display()));
    h.cmd(&format!("e {}", b.display()));
    // vix numbers buffers as: 1 = active, 2..N = parked (oldest first).
    // After two `:e`s, parked = [starter, alpha], so index 2 = starter.
    h.cmd("b 2");
    h.assert_text("starter\n");
}

#[test]
fn ls_picker_lists_all_buffers() {
    let p = tmpfile("two\n");
    let mut h = Harness::with_text("one\n");
    h.cmd(&format!("e {}", p.display()));
    h.cmd("ls");
    assert!(h.picker_open());
}
