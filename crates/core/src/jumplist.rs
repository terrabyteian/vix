use std::collections::VecDeque;
use std::path::PathBuf;

/// One entry in the jump list. Stores line/col rather than a raw char offset
/// so we tolerate the buffer changing between the push and a later Ctrl-O.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpEntry {
    pub path: Option<PathBuf>,
    pub line: usize,
    pub col: usize,
}

const CAP: usize = 100;

/// Vim-style jump list. `pos == entries.len()` means "at the tip, not
/// currently walking back." On the first Ctrl-O from the tip we stash the
/// current cursor as an extra entry so Ctrl-I can return.
#[derive(Default, Debug)]
pub struct JumpList {
    entries: VecDeque<JumpEntry>,
    pos: usize,
}

impl JumpList {
    pub fn push(&mut self, entry: JumpEntry) {
        if self.entries.back() == Some(&entry) {
            self.pos = self.entries.len();
            return;
        }
        self.entries.push_back(entry);
        while self.entries.len() > CAP {
            self.entries.pop_front();
        }
        self.pos = self.entries.len();
    }

    /// Step back. `current` is the cursor position at the time of the Ctrl-O;
    /// we push it on the first step from the tip so Ctrl-I can return.
    pub fn back(&mut self, current: JumpEntry) -> Option<JumpEntry> {
        if self.pos == 0 {
            return None;
        }
        if self.pos == self.entries.len() {
            let already_top = self.entries.back() == Some(&current);
            if !already_top {
                self.entries.push_back(current);
                while self.entries.len() > CAP {
                    self.entries.pop_front();
                }
            }
            self.pos = self.entries.len().saturating_sub(2);
        } else {
            self.pos -= 1;
        }
        self.entries.get(self.pos).cloned()
    }

    /// Step forward.
    pub fn forward(&mut self) -> Option<JumpEntry> {
        if self.pos + 1 >= self.entries.len() {
            return None;
        }
        self.pos += 1;
        self.entries.get(self.pos).cloned()
    }

    pub fn entries(&self) -> impl Iterator<Item = &JumpEntry> {
        self.entries.iter()
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(line: usize) -> JumpEntry {
        JumpEntry { path: Some(PathBuf::from("/t")), line, col: 0 }
    }

    #[test]
    fn push_and_walk_back() {
        let mut j = JumpList::default();
        j.push(entry(1));
        j.push(entry(2));
        j.push(entry(3));
        let got = j.back(entry(10));
        assert_eq!(got, Some(entry(3)));
        assert_eq!(j.back(entry(10)), Some(entry(2)));
        assert_eq!(j.back(entry(10)), Some(entry(1)));
        assert_eq!(j.back(entry(10)), None);
    }

    #[test]
    fn back_from_tip_stashes_current_for_forward() {
        let mut j = JumpList::default();
        j.push(entry(1));
        j.push(entry(2));
        assert_eq!(j.back(entry(5)), Some(entry(2)));
        assert_eq!(j.forward(), Some(entry(5)));
    }

    #[test]
    fn forward_at_tip_is_noop() {
        let mut j = JumpList::default();
        j.push(entry(1));
        assert_eq!(j.forward(), None);
    }

    #[test]
    fn dedup_adjacent() {
        let mut j = JumpList::default();
        j.push(entry(1));
        j.push(entry(1));
        assert_eq!(j.len(), 1);
    }

    #[test]
    fn capacity_trims_oldest() {
        let mut j = JumpList::default();
        for i in 0..(CAP + 10) {
            j.push(entry(i));
        }
        assert_eq!(j.len(), CAP);
        let first = j.entries().next().cloned().unwrap();
        assert_eq!(first.line, 10);
    }

    #[test]
    fn back_then_new_push_appends() {
        let mut j = JumpList::default();
        j.push(entry(1));
        j.push(entry(2));
        j.push(entry(3));
        assert_eq!(j.back(entry(9)), Some(entry(3)));
        assert_eq!(j.back(entry(9)), Some(entry(2)));
        j.push(entry(7));
        assert_eq!(j.back(entry(11)), Some(entry(7)));
    }
}
