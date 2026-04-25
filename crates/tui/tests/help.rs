//! `:help` opens an embedded scratch buffer; `:w` on it refuses; unknown
//! topics surface a friendly error; reopening dedupes by synthetic path.

#![allow(non_snake_case)]

use vix_tui::testing::Harness;

#[test]
fn help_with_no_arg_opens_index() {
    let mut h = Harness::with_text("hello\n");
    h.cmd("help");
    assert!(h.text().contains("vix help"));
    assert!(h.text().contains(":help testing"));
}

#[test]
fn help_testing_opens_manual_doc() {
    let mut h = Harness::with_text("hello\n");
    h.cmd("help testing");
    assert!(h.text().contains("Manual Testing Playbook"));
}

#[test]
fn help_unknown_topic_messages_user() {
    let mut h = Harness::with_text("hello\n");
    h.cmd("help nonsense");
    assert!(h.msg().contains("no help topic"));
    // Original buffer should still be active.
    assert_eq!(h.text(), "hello\n");
}

#[test]
fn help_h_alias_works() {
    let mut h = Harness::with_text("hello\n");
    h.cmd("h testing");
    assert!(h.text().contains("Manual Testing Playbook"));
}

#[test]
fn help_buffer_refuses_save() {
    let mut h = Harness::with_text("hello\n");
    h.cmd("help testing");
    h.cmd("w");
    assert!(h.msg().to_lowercase().contains("scratch"));
}

#[test]
fn help_dedupes_on_reopen() {
    let mut h = Harness::with_text("hello\n");
    let before = h.buffer_count();
    h.cmd("help testing");
    let after_first = h.buffer_count();
    assert_eq!(after_first, before + 1);
    // Switch back to the original buffer, then reopen help — should not
    // create a third buffer.
    h.cmd("bn");
    h.cmd("help testing");
    assert_eq!(h.buffer_count(), after_first);
}

#[test]
fn help_buffer_parks_original() {
    let mut h = Harness::with_text("hello\n");
    h.cmd("help testing");
    // Original buffer is parked — switch back, content intact.
    h.cmd("bn");
    assert_eq!(h.text(), "hello\n");
}
