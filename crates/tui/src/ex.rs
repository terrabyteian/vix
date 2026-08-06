//! Ex command-line: `:` commands (`:w`, `:q`, buffer/picker commands, etc.)
//! and `:s` / `:%s` substitution.

use vix_core::{Change, Transaction};

use crate::Editor;

impl Editor {
    pub(crate) fn run_ex(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        match cmd {
            "w" => self.format_and_save(),
            "fmt" | "format" => {
                self.format_buffer();
            }
            "action" | "actions" | "ca" => {
                self.run_code_action();
            }
            "q" => {
                if self.buffer.dirty() {
                    self.msg = "E37: No write since last change (use :q!)".into();
                } else {
                    self.close_buffer(false);
                }
            }
            "q!" => {
                self.close_buffer(true);
            }
            "qa" | "qall" => {
                if self.any_buffer_dirty() {
                    self.msg = "E37: unsaved buffers exist (use :qa!)".into();
                } else {
                    self.quit = true;
                }
            }
            "qa!" | "qall!" => {
                self.quit = true;
            }
            "wq" | "x" => {
                self.format_and_save();
                if !self.buffer.dirty() {
                    self.close_buffer(false);
                }
            }
            "noh" | "nohl" | "nohlsearch" => {
                self.hl_search = false;
            }
            "preview" | "pv" => {
                if self.view_mode == crate::ViewMode::Rendered {
                    self.msg = "already in the rendered view".into();
                } else {
                    self.toggle_markdown_view();
                }
            }
            "raw" => {
                if self.view_mode == crate::ViewMode::Rendered {
                    self.switch_to_raw_view();
                } else {
                    self.msg = "already in the raw view".into();
                }
            }
            "Files" => {
                self.open_files_picker();
            }
            "Symbols" => {
                self.open_symbols_picker();
            }
            "Buffers" | "ls" => {
                self.open_buffers_picker();
            }
            "jumps" => {
                self.open_jumps_picker();
            }
            "help" | "h" => {
                self.open_help_doc("");
            }
            "bn" | "bnext" => {
                self.next_buffer();
            }
            "bp" | "bprev" | "bprevious" => {
                self.prev_buffer();
            }
            "bd" | "bdelete" => {
                self.close_buffer(false);
            }
            "bd!" | "bdelete!" => {
                self.close_buffer(true);
            }
            "e" | "edit" => {
                self.reload_buffer(false);
            }
            "e!" | "edit!" => {
                self.reload_buffer(true);
            }
            "" => {}
            _ => {
                if let Some(rest) = cmd.strip_prefix("Grep") {
                    self.open_grep_picker(rest.trim());
                } else if let Some(rest) =
                    cmd.strip_prefix("help ").or_else(|| cmd.strip_prefix("h "))
                {
                    self.open_help_doc(rest.trim());
                } else if let Some(rest) =
                    cmd.strip_prefix("e ").or_else(|| cmd.strip_prefix("e! "))
                {
                    let path = std::path::PathBuf::from(rest.trim());
                    self.open_path(&path);
                } else if let Some(rest) = cmd.strip_prefix("b ") {
                    self.switch_buffer_by_spec(rest.trim());
                } else if let Some(new_name) = cmd.strip_prefix("rename ").map(str::trim) {
                    if new_name.is_empty() {
                        self.msg = "usage: :rename <new-name>".into();
                    } else {
                        self.run_rename(new_name);
                    }
                } else if cmd.starts_with("%s") || cmd.starts_with(".s") || cmd.starts_with('s') {
                    self.run_substitute(cmd);
                } else {
                    self.msg = format!("not implemented: :{cmd}");
                }
            }
        }
    }

    /// Parse and execute `:%s/pat/rep/flags`, `:s/pat/rep/flags`, etc.
    /// v1 supports `%` (whole file) and no-range (current line). Flags: g, i.
    pub(crate) fn run_substitute(&mut self, cmd: &str) {
        // Strip the range prefix + the `s`.
        let rest = if let Some(r) = cmd.strip_prefix("%s") {
            r
        } else if let Some(r) = cmd.strip_prefix(".s") {
            r
        } else if let Some(r) = cmd.strip_prefix('s') {
            r
        } else {
            self.msg = "internal: bad :s".into();
            return;
        };
        let whole_file = cmd.starts_with("%s");

        // First char after `s` is the delimiter.
        let mut chars = rest.chars();
        let Some(delim) = chars.next() else {
            self.msg = "E471: usage :s/pat/rep/flags".into();
            return;
        };
        let body: String = chars.collect();
        let parts: Vec<&str> = body.splitn(3, delim).collect();
        if parts.len() < 2 {
            self.msg = "E471: usage :s/pat/rep/flags".into();
            return;
        }
        let pattern = parts[0];
        let replacement = parts[1];
        let flags = parts.get(2).copied().unwrap_or("");
        let global = flags.contains('g');
        let case_insensitive = flags.contains('i');

        let re = match regex::RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                self.msg = format!("E: {e}");
                return;
            }
        };

        // Determine the char-range to operate on.
        let (range_start, range_end) = if whole_file {
            (0, self.buffer.len_chars())
        } else {
            let line = self.cursor_line();
            let s = self.buffer.line_to_char(line);
            let e = s + self.buffer.line_len_chars(line);
            (s, e)
        };

        // Collect matches line-by-line so we can replace inside each line with
        // Vim-ish semantics (per-line `g` flag). Must walk lines forward but
        // apply replacements in reverse across the whole buffer so offsets
        // stay valid.
        let (start_line, _) = self.buffer.char_to_line_col(range_start);
        let end_line_exclusive = if range_end == 0 {
            0
        } else {
            self.buffer.char_to_line_col(range_end.saturating_sub(1)).0 + 1
        };

        let mut replacements: Vec<(std::ops::Range<usize>, String, String)> = Vec::new();
        for line in start_line..end_line_exclusive {
            let line_text: String = self.buffer.rope().line(line).chars().collect();
            let line_text = line_text.trim_end_matches('\n');
            let line_start = self.buffer.line_to_char(line);
            let mut offset = 0usize; // byte offset within line_text
            for m in re.find_iter(line_text) {
                let start_byte = m.start();
                let end_byte = m.end();
                let matched = &line_text[start_byte..end_byte];
                let rep = re.replace(matched, replacement).to_string();
                let start_char = line_start + line_text[..start_byte].chars().count();
                let end_char = line_start + line_text[..end_byte].chars().count();
                replacements.push((start_char..end_char, matched.to_string(), rep));
                offset = end_byte;
                if !global {
                    break;
                }
            }
            let _ = offset;
        }

        if replacements.is_empty() {
            self.msg = format!("E486: Pattern not found: {pattern}");
            return;
        }

        // Apply in reverse char-offset order so earlier offsets stay valid.
        replacements.sort_by_key(|r| std::cmp::Reverse(r.0.start));
        let sel_before = self.sel;
        let mut tx = Transaction::new();
        tx.sel_before = Some(sel_before);
        let count = replacements.len();
        for (range, old, new_text) in &replacements {
            self.buffer.remove_range(range.clone());
            self.buffer.insert_str(range.start, new_text);
            tx.push(Change::Delete {
                at: range.start,
                removed: old.clone(),
            });
            tx.push(Change::Insert {
                at: range.start,
                text: new_text.clone(),
            });
        }
        self.sel = self.sel.clamped(&self.buffer);
        tx.sel_after = Some(self.sel);
        self.history.commit(tx);
        self.msg = format!("{count} substitutions");
    }
}
