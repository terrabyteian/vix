//! Insert-mode completion popup: prefix tracking, filtering, and accept/
//! navigate actions. Popup state; requests to the LSP server live in
//! `crate::lsp`.

use crate::Editor;

/// A pending completion popup in Insert mode.
#[derive(Clone, Debug, Default)]
pub(crate) struct CompletionPopup {
    /// Full list from the server.
    pub(crate) items: Vec<vix_lsp::lsp_types::CompletionItem>,
    /// Indices into `items` that match the current prefix (case-insensitive).
    pub(crate) visible: Vec<usize>,
    /// Cursor within `visible`.
    pub(crate) selected: usize,
    /// Char offset of the identifier's first char.
    pub(crate) prefix_start: usize,
}

impl Editor {
    /// Walk back from `at` over identifier chars (alphanumeric or `_`) to find
    /// the start of the word under the cursor.
    pub(crate) fn word_prefix_start(&self, at: usize) -> usize {
        let rope = self.buffer.rope();
        let mut i = at;
        while i > 0 {
            let c = rope.char(i - 1);
            if c.is_alphanumeric() || c == '_' {
                i -= 1;
            } else {
                break;
            }
        }
        i
    }

    /// Rebuild `visible` based on the current prefix (chars between
    /// `prefix_start` and the cursor), case-insensitive prefix match.
    pub(crate) fn refilter_completions(&mut self) {
        let Some(popup) = self.completion_popup.as_mut() else {
            return;
        };
        if self.sel.head < popup.prefix_start {
            self.completion_popup = None;
            return;
        }
        let prefix: String = self
            .buffer
            .rope()
            .slice(popup.prefix_start..self.sel.head)
            .to_string();
        let prefix_lc = prefix.to_lowercase();
        popup.visible.clear();
        for (idx, item) in popup.items.iter().enumerate() {
            let hay = item.filter_text.as_deref().unwrap_or(item.label.as_str());
            if hay.to_lowercase().starts_with(&prefix_lc) {
                popup.visible.push(idx);
            }
        }
        if popup.visible.is_empty() {
            self.completion_popup = None;
        } else {
            popup.selected = popup.selected.min(popup.visible.len() - 1);
        }
    }

    /// Apply the currently-selected completion item: replace the prefix range
    /// with the item's text.
    pub(crate) fn accept_completion(&mut self) {
        let Some(popup) = self.completion_popup.take() else {
            return;
        };
        let Some(&item_idx) = popup.visible.get(popup.selected) else {
            return;
        };
        let item = &popup.items[item_idx];
        let insert_text = item
            .insert_text
            .clone()
            .unwrap_or_else(|| item.label.clone());
        // Replace [prefix_start, cursor) with insert_text via the pending
        // insert session so undo groups this with the rest of the insert.
        let end = self.sel.head;
        let start = popup.prefix_start.min(end);
        // Delete backwards from cursor to prefix_start
        for _ in start..end {
            self.backspace_in_session();
        }
        for c in insert_text.chars() {
            self.insert_char_in_session(c);
        }
    }

    /// Select the next / previous completion item. Wraps.
    pub(crate) fn move_completion_selection(&mut self, delta: isize) {
        let Some(popup) = self.completion_popup.as_mut() else {
            return;
        };
        if popup.visible.is_empty() {
            return;
        }
        let len = popup.visible.len() as isize;
        let cur = popup.selected as isize;
        let new = ((cur + delta) % len + len) % len;
        popup.selected = new as usize;
    }
}
