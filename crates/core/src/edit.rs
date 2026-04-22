//! Transactions and undo history.
//!
//! A `Change` is a single primitive edit (insert or delete). A `Transaction`
//! bundles changes that should undo together — e.g., an entire Insert-mode
//! session, or an operator application like `dw`.
//!
//! Design choices:
//! - Changes store both the inserted text AND the removed text, so undo is
//!   symmetric (inverted change).
//! - Cursor position before/after is stored on the Transaction so undo/redo
//!   restore the cursor where the user expects.
//! - History is a flat stack (last-write-wins on redo) — not a true tree.
//!   A branching undo tree can come later; flat stack matches Vim `u`/`Ctrl-R`.

use crate::buffer::Buffer;
use crate::keymap::InsertPos;
use crate::mode::PendingOp;
use crate::motion::Motion;
use crate::selection::Selection;
use crate::textobject::{TextObject, TextObjectKind};

/// A replayable editing intent, recorded by the dot-repeat mechanism.
///
/// Intent is recorded at the "resolved" level — after counts and operators
/// are merged — so replay just means re-dispatching the same action at the
/// current cursor. This keeps `.` stable across LSP-driven buffer changes.
#[derive(Debug, Clone)]
pub enum RepeatAction {
    /// `i/I/a/A/o/O<text><Esc>` — enter insert mode a particular way, then type text.
    InsertBurst { pos: InsertPos, text: String },
    /// `dw`, `yw`, `>j` — operator over a motion.
    Operate { op: PendingOp, motion: Motion, count: usize },
    /// `dd`, `yy`, `<<` — operator on whole lines.
    OperateLine { op: PendingOp, count: usize },
    /// `diw`, `ci"`, `ya(` — operator over a text object.
    OperateObject { op: PendingOp, object: TextObject, kind: TextObjectKind, count: usize },
    /// `x` / `X` — delete chars on the current line.
    DeleteChars { forward: bool, count: usize },
}

#[derive(Debug, Clone)]
pub enum Change {
    /// Insert `text` at char offset `at`.
    Insert { at: usize, text: String },
    /// Delete the text `removed` that currently sits at `at..(at+removed.len_chars)`.
    Delete { at: usize, removed: String },
}

impl Change {
    fn apply(&self, buf: &mut Buffer) {
        match self {
            Change::Insert { at, text } => buf.insert_str(*at, text),
            Change::Delete { at, removed } => {
                let n = removed.chars().count();
                buf.remove_range(*at..(*at + n));
            }
        }
    }

    fn invert(&self) -> Change {
        match self {
            Change::Insert { at, text } => Change::Delete { at: *at, removed: text.clone() },
            Change::Delete { at, removed } => Change::Insert { at: *at, text: removed.clone() },
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Transaction {
    pub changes: Vec<Change>,
    pub sel_before: Option<Selection>,
    pub sel_after: Option<Selection>,
}

impl Transaction {
    pub fn new() -> Self { Self::default() }

    pub fn is_empty(&self) -> bool { self.changes.is_empty() }

    pub fn push(&mut self, c: Change) { self.changes.push(c); }

    /// Apply this transaction to the buffer.
    pub fn apply(&self, buf: &mut Buffer) {
        for c in &self.changes { c.apply(buf); }
    }

    /// Return the inverse transaction (for undo).
    pub fn invert(&self) -> Transaction {
        Transaction {
            // Inverted changes must be applied in reverse order: if we inserted
            // "a" then "b", undoing means removing "b" first.
            changes: self.changes.iter().rev().map(Change::invert).collect(),
            sel_before: self.sel_after,
            sel_after: self.sel_before,
        }
    }
}

#[derive(Debug, Default)]
pub struct History {
    /// Committed transactions, most recent last.
    done: Vec<Transaction>,
    /// Transactions that were undone and can be redone, most recent last.
    undone: Vec<Transaction>,
}

impl History {
    pub fn new() -> Self { Self::default() }

    /// Record a completed transaction. Clears the redo stack.
    pub fn commit(&mut self, tx: Transaction) {
        if tx.is_empty() { return; }
        self.done.push(tx);
        self.undone.clear();
    }

    /// Undo the last transaction. Returns the inverse applied, along with the
    /// selection to restore (the selection *before* the original edit).
    pub fn undo(&mut self, buf: &mut Buffer) -> Option<Selection> {
        let tx = self.done.pop()?;
        let inv = tx.invert();
        inv.apply(buf);
        let restore = tx.sel_before;
        self.undone.push(tx);
        restore
    }

    pub fn redo(&mut self, buf: &mut Buffer) -> Option<Selection> {
        let tx = self.undone.pop()?;
        tx.apply(buf);
        let restore = tx.sel_after;
        self.done.push(tx);
        restore
    }

    pub fn can_undo(&self) -> bool { !self.done.is_empty() }
    pub fn can_redo(&self) -> bool { !self.undone.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_redo_roundtrip() {
        let mut buf = Buffer::from_text("hello");
        let mut hist = History::new();

        let mut tx = Transaction::new();
        tx.sel_before = Some(Selection::at(5));
        tx.push(Change::Insert { at: 5, text: " world".into() });
        tx.sel_after = Some(Selection::at(11));
        tx.apply(&mut buf);
        hist.commit(tx);

        assert_eq!(buf.rope().to_string(), "hello world");

        let sel = hist.undo(&mut buf).unwrap();
        assert_eq!(buf.rope().to_string(), "hello");
        assert_eq!(sel.head, 5);

        let sel = hist.redo(&mut buf).unwrap();
        assert_eq!(buf.rope().to_string(), "hello world");
        assert_eq!(sel.head, 11);
    }

    #[test]
    fn undo_multi_change_transaction() {
        let mut buf = Buffer::from_text("abc");
        let mut hist = History::new();

        let mut tx = Transaction::new();
        tx.sel_before = Some(Selection::at(0));
        tx.push(Change::Delete { at: 0, removed: "a".into() });
        tx.push(Change::Insert { at: 0, text: "X".into() });
        tx.sel_after = Some(Selection::at(1));
        tx.apply(&mut buf);
        hist.commit(tx);

        assert_eq!(buf.rope().to_string(), "Xbc");
        hist.undo(&mut buf);
        assert_eq!(buf.rope().to_string(), "abc");
    }

    #[test]
    fn commit_clears_redo() {
        let mut buf = Buffer::from_text("a");
        let mut hist = History::new();

        let mut tx1 = Transaction::new();
        tx1.push(Change::Insert { at: 1, text: "b".into() });
        tx1.apply(&mut buf);
        hist.commit(tx1);

        hist.undo(&mut buf);
        assert!(hist.can_redo());

        let mut tx2 = Transaction::new();
        tx2.push(Change::Insert { at: 1, text: "c".into() });
        tx2.apply(&mut buf);
        hist.commit(tx2);

        assert!(!hist.can_redo(), "new commit should clear the redo stack");
    }
}
