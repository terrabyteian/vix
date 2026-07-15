//! In-buffer search: `/`, `?`, `n`/`N`, and `*`/`#` (word-under-cursor).

use vix_core::{compile_search, find_backward, find_forward, Case, SearchDirection, Selection};

use crate::Editor;

impl Editor {
    /// Returns the line index of the match on success so the caller can
    /// adjust the viewport (e.g. pin the first match to the top of the
    /// pane on a fresh `/` search).
    pub(crate) fn do_search(&mut self, query: &str, dir: SearchDirection) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        let re = match compile_search(query, Case::Smart) {
            Ok(r) => r,
            Err(e) => {
                self.msg = format!("E: {e}");
                return None;
            }
        };
        // Vim starts search from cursor + 1 for forward, cursor for backward.
        let start_from = match dir {
            SearchDirection::Forward => (self.sel.head + 1).min(self.buffer.len_chars()),
            SearchDirection::Backward => self.sel.head,
        };
        let hit = match dir {
            SearchDirection::Forward => find_forward(&self.buffer, &re, start_from)
                .or_else(|| find_forward(&self.buffer, &re, 0)), // wrap
            SearchDirection::Backward => find_backward(&self.buffer, &re, start_from)
                .or_else(|| find_backward(&self.buffer, &re, self.buffer.len_chars())),
        };
        match hit {
            Some((s, _)) => {
                self.sel = Selection::at(s).clamped(&self.buffer);
                self.last_search = Some((query.to_string(), dir));
                self.hl_search = true;
                Some(self.cursor_line())
            }
            None => {
                self.msg = format!("E486: Pattern not found: {query}");
                None
            }
        }
    }

    pub(crate) fn word_search_under(&mut self, dir: SearchDirection) {
        let rope = self.buffer.rope();
        let len = self.buffer.len_chars();
        if len == 0 {
            return;
        }
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let mut start = self.sel.head.min(len.saturating_sub(1));
        // If cursor is not on a word char, try to find one to the right on this line.
        if !is_word(rope.char(start)) {
            let (line, _) = self.buffer.char_to_line_col(start);
            let line_end = self.buffer.line_to_char(line) + self.buffer.line_len_chars(line);
            let mut i = start;
            while i < line_end && !is_word(rope.char(i)) {
                i += 1;
            }
            if i >= line_end {
                self.msg = "E348: No string under cursor".into();
                return;
            }
            start = i;
        }
        // Extend backward to start of word.
        while start > 0 && is_word(rope.char(start - 1)) {
            start -= 1;
        }
        let mut end = start;
        while end < len && is_word(rope.char(end)) {
            end += 1;
        }
        let word: String = rope.slice(start..end).to_string();
        // Build a pattern with word boundaries, escaping regex metachars.
        let escaped = regex_escape_like(&word);
        let pattern = format!(r"\b{escaped}\b");
        self.do_search(&pattern, dir);
    }

    pub(crate) fn search_repeat(&mut self, dir: SearchDirection) {
        let Some((query, last_dir)) = self.last_search.clone() else {
            self.msg = "No previous search".into();
            return;
        };
        // `n` repeats in original direction; `N` reverses it.
        let effective = match (last_dir, dir) {
            (d, SearchDirection::Forward) => d,
            (SearchDirection::Forward, SearchDirection::Backward) => SearchDirection::Backward,
            (SearchDirection::Backward, SearchDirection::Backward) => SearchDirection::Forward,
        };
        self.hl_search = true;
        self.do_search(&query, effective);
    }
}

pub(crate) fn regex_escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".+*?()[]{}|^$\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
