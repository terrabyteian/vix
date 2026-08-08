//! LSP integration: request/response plumbing, code actions, rename,
//! formatting, and completion requests (completion popup state lives in
//! `crate::completion`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vix_core::{Buffer, Change, Mode, Selection, Transaction};
use vix_lsp::lsp_types::{
    DiagnosticSeverity, GotoDefinitionResponse, Hover, HoverContents, Location, MarkedString, Uri,
};
use vix_lsp::{parse_response, path_to_uri, server_for_path, uri_to_path, LspClient, ServerEvent};
use vix_picker::Utf32String;

use crate::completion::CompletionPopup;
use crate::picker::{Picker, PickerItem, PickerKind, PickerValue};
use crate::Editor;

/// What we asked for — lets us interpret the response when it arrives.
#[derive(Clone, Debug)]
pub(crate) enum PendingRequest {
    Hover,
    Definition,
    /// Completion request. `prefix_start` is the char offset where the
    /// identifier under the cursor began when we sent the request, so we
    /// know what range to replace on accept.
    Completion {
        prefix_start: usize,
    },
}

/// Per-document LSP state. We stamp documents with a separate version number
/// from the buffer's mutation counter so we only emit `didChange` on real
/// text edits.
#[derive(Clone)]
pub(crate) struct LspDocState {
    pub(crate) uri: Uri,
    /// The LSP-visible document version. Starts at 1 on `didOpen`, bumped
    /// on every `didChange`.
    pub(crate) version: i32,
    /// Snapshot of `Buffer::version()` at the last `didChange`. Used to
    /// gate re-sync — we only push changes when this lags.
    pub(crate) last_sent_buffer_version: u64,
    /// Which server cmd owns this doc.
    pub(crate) server_cmd: String,
}

impl Editor {
    /// Ensure an LSP server is running for the active buffer's language, and
    /// that the buffer is open on it. Idempotent per (server, path).
    pub(crate) fn ensure_lsp_open(&mut self) {
        let Some(path) = self.buffer.path() else {
            return;
        };
        let path: PathBuf = path.to_path_buf();
        let Some(config) = server_for_path(&path) else {
            return;
        };
        // Spawn the server if we haven't already.
        let cmd = config.cmd.clone();
        if self.lsp_failed.contains(&cmd) {
            return;
        }
        if !self.lsp_clients.contains_key(&cmd) {
            match LspClient::start(config, &self.root) {
                Ok(c) => {
                    self.lsp_clients.insert(cmd.clone(), c);
                }
                Err(e) => {
                    self.msg = format!("lsp: {e}");
                    self.lsp_failed.insert(cmd);
                    return;
                }
            }
        }
        // Open the document if we haven't already. `didChange` batching handles
        // subsequent edits via `sync_lsp_changes`.
        if self.lsp_docs.contains_key(&path) {
            return;
        }
        let Ok(uri) = path_to_uri(&path) else {
            self.msg = "lsp: bad path".into();
            return;
        };
        let text = self.buffer.rope().to_string();
        if let Some(client) = self.lsp_clients.get(&cmd) {
            client.did_open(uri.clone(), 1, text);
        }
        self.lsp_docs.insert(
            path,
            LspDocState {
                uri,
                version: 1,
                last_sent_buffer_version: self.buffer.version(),
                server_cmd: cmd,
            },
        );
    }

    /// If the active buffer's LSP doc version lags the buffer's mutation
    /// counter, send a full `didChange` and bump. Full-text sync is
    /// heavier than incremental but dead-simple; incremental can come later.
    pub(crate) fn sync_lsp_changes(&mut self) {
        let Some(path) = self.buffer.path() else {
            return;
        };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get_mut(&path) else {
            return;
        };
        let bv = self.buffer.version();
        if doc.last_sent_buffer_version == bv {
            return;
        }
        doc.version += 1;
        doc.last_sent_buffer_version = bv;
        let uri = doc.uri.clone();
        let version = doc.version;
        let cmd = doc.server_cmd.clone();
        let text = self.buffer.rope().to_string();
        if let Some(client) = self.lsp_clients.get(&cmd) {
            client.did_change_full(uri, version, text);
        }
    }

    /// Drain any pending events from all running LSP clients.
    pub(crate) fn drain_lsp_events(&mut self) {
        // Collect first (separate borrows) then dispatch.
        let mut batch: Vec<(String, ServerEvent)> = Vec::new();
        for (cmd, client) in &self.lsp_clients {
            while let Some(ev) = client.try_recv() {
                batch.push((cmd.clone(), ev));
            }
        }
        if !batch.is_empty() {
            // Diagnostics, hover text, completions, workspace edits — every
            // event variant can change something on screen.
            self.request_redraw();
        }
        for (cmd, ev) in batch {
            self.handle_lsp_event(cmd, ev);
        }
    }

    pub(crate) fn handle_lsp_event(&mut self, cmd: String, ev: ServerEvent) {
        match ev {
            ServerEvent::Diagnostics { uri, diagnostics } => {
                if let Some(path) = uri_to_path(&uri) {
                    if diagnostics.is_empty() {
                        self.diagnostics.remove(&path);
                    } else {
                        self.diagnostics.insert(path, diagnostics);
                    }
                    self.diagnostics_gen = self.diagnostics_gen.wrapping_add(1);
                }
            }
            ServerEvent::Response { id, result, error } => {
                if let Some(intent) = self.pending_requests.remove(&(cmd, id)) {
                    self.handle_lsp_response(intent, result, error);
                }
            }
            ServerEvent::Log {
                level: _,
                message: _,
            } => {
                // Stash the most recent server log for debugging; drop in msg
                // only if nothing more useful is present.
            }
            ServerEvent::Show { level: _, message } => {
                self.msg = message;
            }
            ServerEvent::Exited => {
                // Tear down the dead client and drop its doc state; the next
                // ensure_lsp_open will respawn if we're still under budget.
                self.lsp_clients.remove(&cmd);
                self.lsp_docs.retain(|_, doc| doc.server_cmd != cmd);
                self.pending_requests.retain(|(c, _), _| c != &cmd);
                let now = Instant::now();
                let entry = self.lsp_restart_log.entry(cmd.clone()).or_default();
                entry.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
                if entry.len() >= 3 {
                    self.lsp_failed.insert(cmd.clone());
                    self.msg = format!("lsp {cmd}: crashed 3x in 60s — giving up");
                } else {
                    entry.push(now);
                    self.msg = format!("lsp {cmd}: crashed, will restart on next edit");
                }
            }
        }
    }

    pub(crate) fn handle_lsp_response(
        &mut self,
        intent: PendingRequest,
        result: Option<vix_lsp::Value>,
        error: Option<String>,
    ) {
        if let Some(e) = error {
            self.msg = format!("lsp: {e}");
            return;
        }
        match intent {
            PendingRequest::Hover => match parse_response::<Hover>(result) {
                Ok(Some(h)) => {
                    let text = hover_text(&h);
                    if text.trim().is_empty() {
                        self.msg = "no hover info".into();
                    } else {
                        self.hover_popup = Some(text);
                    }
                }
                Ok(None) => self.msg = "no hover info".into(),
                Err(e) => self.msg = format!("lsp hover: {e}"),
            },
            PendingRequest::Definition => match parse_response::<GotoDefinitionResponse>(result) {
                Ok(Some(resp)) => self.jump_to_definition(resp),
                Ok(None) => self.msg = "no definition".into(),
                Err(e) => self.msg = format!("lsp definition: {e}"),
            },
            PendingRequest::Completion { prefix_start } => {
                match parse_response::<vix_lsp::lsp_types::CompletionResponse>(result) {
                    Ok(Some(resp)) => {
                        use vix_lsp::lsp_types::CompletionResponse;
                        let items = match resp {
                            CompletionResponse::Array(v) => v,
                            CompletionResponse::List(l) => l.items,
                        };
                        if items.is_empty() {
                            self.completion_popup = None;
                            self.msg = "no completions".into();
                        } else {
                            // Only install the popup if we're still in Insert
                            // and the cursor hasn't moved before prefix_start.
                            if self.mode == Mode::Insert && self.sel.head >= prefix_start {
                                self.completion_popup = Some(CompletionPopup {
                                    items,
                                    visible: Vec::new(),
                                    selected: 0,
                                    prefix_start,
                                });
                                self.refilter_completions();
                            }
                        }
                    }
                    Ok(None) => {
                        self.completion_popup = None;
                        self.msg = "no completions".into();
                    }
                    Err(e) => self.msg = format!("lsp completion: {e}"),
                }
            }
        }
    }

    pub(crate) fn jump_to_definition(&mut self, resp: GotoDefinitionResponse) {
        let loc: Option<Location> = match resp {
            GotoDefinitionResponse::Scalar(l) => Some(l),
            GotoDefinitionResponse::Array(mut xs) => xs.drain(..).next(),
            GotoDefinitionResponse::Link(mut xs) => xs.drain(..).next().map(|l| Location {
                uri: l.target_uri,
                range: l.target_selection_range,
            }),
        };
        let Some(loc) = loc else {
            self.msg = "no definition".into();
            return;
        };
        let Some(path) = uri_to_path(&loc.uri) else {
            self.msg = "lsp: non-file target".into();
            return;
        };
        // Record the departure even if we're jumping within the same buffer,
        // so `Ctrl-O` returns to the call site after `gd`.
        let same_buf = self
            .buffer
            .path()
            .map(|p| p == path.as_path())
            .unwrap_or(false);
        if same_buf {
            self.push_jump();
        }
        // `open_path` already records departure when switching buffers.
        self.open_path(&path);
        let line = loc.range.start.line as usize;
        let ch = loc.range.start.character as usize;
        let line = line.min(self.buffer.len_lines().saturating_sub(1));
        let line_start = self.buffer.line_to_char(line);
        let line_len = self.buffer.line_len_chars(line);
        let offset = line_start + ch.min(line_len);
        self.sel = Selection::at(offset).clamped(&self.buffer);
    }

    /// Send a hover request at the current cursor. No-op if the buffer isn't
    /// bound to an LSP server.
    pub(crate) fn request_hover(&mut self) {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else {
            return;
        };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else {
            self.msg = "lsp: no server".into();
            return;
        };
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        if let Some(client) = self.lsp_clients.get(&doc.server_cmd) {
            let id = client.hover(doc.uri, line as u32, col as u32);
            self.pending_requests
                .insert((doc.server_cmd, id), PendingRequest::Hover);
        }
    }

    /// Send a goto-definition request at the current cursor.
    pub(crate) fn request_definition(&mut self) {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else {
            return;
        };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else {
            self.msg = "lsp: no server".into();
            return;
        };
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        if let Some(client) = self.lsp_clients.get(&doc.server_cmd) {
            let id = client.definition(doc.uri, line as u32, col as u32);
            self.pending_requests
                .insert((doc.server_cmd, id), PendingRequest::Definition);
        }
    }

    /// Send a completion request from the current cursor. Records the word
    /// prefix start so we know what range to replace when the user accepts.
    pub(crate) fn request_completion(&mut self) {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else {
            return;
        };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else {
            self.msg = "lsp: no server".into();
            return;
        };
        let prefix_start = self.word_prefix_start(self.sel.head);
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        if let Some(client) = self.lsp_clients.get(&doc.server_cmd) {
            let id = client.completion(doc.uri, line as u32, col as u32);
            self.pending_requests.insert(
                (doc.server_cmd, id),
                PendingRequest::Completion { prefix_start },
            );
        }
    }

    /// Request code actions at the current cursor + present a picker. On
    /// accept the chosen action is applied (edit and/or command).
    pub(crate) fn run_code_action(&mut self) {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else {
            self.msg = "no file".into();
            return;
        };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else {
            self.msg = "lsp: no server".into();
            return;
        };
        let Some(client) = self.lsp_clients.get(&doc.server_cmd) else {
            return;
        };
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        // Range: if we're in a Visual selection, use it; otherwise a
        // zero-width range at the cursor. Diagnostics on the cursor's line
        // are passed as context so the server knows what to suggest.
        let range = match self.mode {
            Mode::Visual | Mode::VisualLine => {
                let r = self.visual_range();
                let (sl, sc) = self.buffer.char_to_line_col(r.start);
                let (el, ec) = self.buffer.char_to_line_col(r.end);
                vix_lsp::lsp_types::Range {
                    start: vix_lsp::lsp_types::Position {
                        line: sl as u32,
                        character: sc as u32,
                    },
                    end: vix_lsp::lsp_types::Position {
                        line: el as u32,
                        character: ec as u32,
                    },
                }
            }
            _ => vix_lsp::lsp_types::Range {
                start: vix_lsp::lsp_types::Position {
                    line: line as u32,
                    character: col as u32,
                },
                end: vix_lsp::lsp_types::Position {
                    line: line as u32,
                    character: col as u32,
                },
            },
        };
        let diags: Vec<_> = self
            .diagnostics
            .get(&path)
            .map(|v| {
                v.iter()
                    .filter(|d| {
                        let s = d.range.start.line as usize;
                        let e = d.range.end.line as usize;
                        s <= line && line <= e
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let id = client.code_action(doc.uri, range, diags);
        let result = match client.wait_response(id, Duration::from_millis(3000)) {
            Some((res, None)) => res,
            Some((_, Some(e))) => {
                self.msg = format!("code action: {e}");
                return;
            }
            None => {
                self.msg = "code action: timed out".into();
                return;
            }
        };
        let Some(result) = result else {
            self.msg = "no code actions".into();
            return;
        };
        let actions: Vec<vix_lsp::lsp_types::CodeActionOrCommand> =
            match parse_response(Some(result)) {
                Ok(Some(a)) => a,
                _ => {
                    self.msg = "no code actions".into();
                    return;
                }
            };
        if actions.is_empty() {
            self.msg = "no code actions".into();
            return;
        }
        // Build picker items. Value is the index into `pending_code_actions`.
        let mut items: Vec<PickerItem> = Vec::with_capacity(actions.len());
        for (i, act) in actions.iter().enumerate() {
            let title = match act {
                vix_lsp::lsp_types::CodeActionOrCommand::Command(c) => c.title.clone(),
                vix_lsp::lsp_types::CodeActionOrCommand::CodeAction(a) => a.title.clone(),
            };
            let haystack = Utf32String::from(title.as_str());
            items.push(PickerItem {
                display: title,
                value: PickerValue::CodeAction(i),
                haystack,
            });
        }
        self.pending_code_actions = actions;
        self.picker = Some(Picker::new(PickerKind::CodeActions, items));
    }

    /// Apply a selected code action: first its WorkspaceEdit (if any), then
    /// its Command (best-effort — we log unknown commands rather than round-
    /// tripping `workspace/executeCommand`).
    pub(crate) fn apply_code_action(&mut self, idx: usize) {
        let Some(action) = self.pending_code_actions.get(idx).cloned() else {
            return;
        };
        match action {
            vix_lsp::lsp_types::CodeActionOrCommand::Command(cmd) => {
                self.run_lsp_command(cmd);
            }
            vix_lsp::lsp_types::CodeActionOrCommand::CodeAction(a) => {
                if let Some(edit) = a.edit {
                    self.apply_workspace_edit(edit);
                }
                if let Some(cmd) = a.command {
                    self.run_lsp_command(cmd);
                }
            }
        }
        self.pending_code_actions.clear();
    }

    /// Send `workspace/executeCommand`. Fire-and-forget; most rust-analyzer
    /// commands just bounce back a WorkspaceEdit via `workspace/applyEdit`,
    /// which we don't currently handle — but the edit part of most actions
    /// is already returned inline in the CodeAction and applied above.
    pub(crate) fn run_lsp_command(&mut self, cmd: vix_lsp::lsp_types::Command) {
        let Some(path) = self.buffer.path() else {
            return;
        };
        let Some(doc) = self.lsp_docs.get(path).cloned() else {
            return;
        };
        let Some(client) = self.lsp_clients.get(&doc.server_cmd) else {
            return;
        };
        let _ = client.execute_command(cmd.command, cmd.arguments.unwrap_or_default());
    }

    /// Send `textDocument/rename` at the cursor, wait for the server's
    /// WorkspaceEdit, and apply it across all affected files. Files already
    /// open (active or parked) are edited in-place; files only on disk are
    /// loaded, edited, and written back.
    pub(crate) fn run_rename(&mut self, new_name: &str) {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else {
            self.msg = "lsp: no file".into();
            return;
        };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else {
            self.msg = "lsp: no server".into();
            return;
        };
        let Some(client) = self.lsp_clients.get(&doc.server_cmd) else {
            return;
        };
        let (line, col) = self.buffer.char_to_line_col(self.sel.head);
        let id = client.rename(
            doc.uri.clone(),
            line as u32,
            col as u32,
            new_name.to_string(),
        );
        let result = match client.wait_response(id, Duration::from_millis(5000)) {
            Some((res, None)) => res,
            Some((_, Some(e))) => {
                self.msg = format!("rename: {e}");
                return;
            }
            None => {
                self.msg = "rename: timed out (server still indexing?)".into();
                return;
            }
        };
        let Some(result) = result else {
            self.msg = "rename: not renamable at this position".into();
            return;
        };
        let edit: vix_lsp::lsp_types::WorkspaceEdit = match parse_response(Some(result.clone())) {
            Ok(Some(e)) => e,
            Ok(None) => {
                self.msg = "rename: not renamable at this position".into();
                return;
            }
            Err(e) => {
                self.msg = format!("rename: bad response: {e}");
                return;
            }
        };
        self.apply_workspace_edit(edit);
    }

    /// Apply a WorkspaceEdit across active, parked, and on-disk files.
    /// Servers may send either the legacy `changes` map or the newer
    /// `documentChanges` list — we flatten both to `(Uri, Vec<TextEdit>)`.
    pub(crate) fn apply_workspace_edit(&mut self, edit: vix_lsp::lsp_types::WorkspaceEdit) {
        use vix_lsp::lsp_types::{DocumentChangeOperation, DocumentChanges, OneOf};

        let mut per_file: Vec<(vix_lsp::lsp_types::Uri, Vec<vix_lsp::lsp_types::TextEdit>)> =
            Vec::new();
        if let Some(changes) = edit.changes {
            for (uri, edits) in changes {
                per_file.push((uri, edits));
            }
        }
        if let Some(doc_changes) = edit.document_changes {
            match doc_changes {
                DocumentChanges::Edits(edits) => {
                    for tde in edits {
                        let plain: Vec<_> = tde
                            .edits
                            .into_iter()
                            .map(|e| match e {
                                OneOf::Left(te) => te,
                                OneOf::Right(ann) => ann.text_edit,
                            })
                            .collect();
                        per_file.push((tde.text_document.uri, plain));
                    }
                }
                DocumentChanges::Operations(ops) => {
                    for op in ops {
                        if let DocumentChangeOperation::Edit(tde) = op {
                            let plain: Vec<_> = tde
                                .edits
                                .into_iter()
                                .map(|e| match e {
                                    OneOf::Left(te) => te,
                                    OneOf::Right(ann) => ann.text_edit,
                                })
                                .collect();
                            per_file.push((tde.text_document.uri, plain));
                        }
                    }
                }
            }
        }

        if per_file.is_empty() {
            self.msg = "rename: server returned no edits".into();
            return;
        }

        let mut files_touched = 0usize;
        let mut total_edits = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for (uri, edits) in per_file {
            let Some(path) = uri_to_path(&uri) else {
                errors.push(format!("non-file uri: {}", uri.as_str()));
                continue;
            };
            total_edits += edits.len();
            if self.apply_edits_to_any_buffer(&path, &edits, &mut errors) {
                files_touched += 1;
            }
        }
        let mut msg = format!("renamed in {files_touched} file(s), {total_edits} edit(s)");
        if !errors.is_empty() {
            msg.push_str(" — errors: ");
            msg.push_str(&errors.join("; "));
        }
        self.msg = msg;
    }

    /// Apply `edits` to the buffer at `path`. If it's the active buffer, edit
    /// in place. If parked, edit the parked copy. If not loaded, read from
    /// disk, apply, and write back.
    pub(crate) fn apply_edits_to_any_buffer(
        &mut self,
        path: &std::path::Path,
        edits: &[vix_lsp::lsp_types::TextEdit],
        errors: &mut Vec<String>,
    ) -> bool {
        if self.buffer.path() == Some(path) {
            self.apply_text_edits(edits);
            return true;
        }
        if let Some(pos) = self
            .other_buffers
            .iter()
            .position(|b| b.buffer.path() == Some(path))
        {
            let save = &mut self.other_buffers[pos];
            let mut tx = Transaction::new();
            tx.sel_before = Some(save.sel);
            apply_text_edits_to_buffer_tx(&mut save.buffer, edits, &mut tx);
            save.sel = save.sel.clamped(&save.buffer);
            tx.sel_after = Some(save.sel);
            if !tx.is_empty() {
                save.history.commit(tx);
            }
            return true;
        }
        // Not loaded: read, edit, write. No history to commit to — the
        // Transaction is built and discarded.
        match Buffer::load(path) {
            Ok(mut buf) => {
                let mut throwaway = Transaction::new();
                apply_text_edits_to_buffer_tx(&mut buf, edits, &mut throwaway);
                if let Err(e) = buf.save() {
                    errors.push(format!("write {}: {e}", path.display()));
                    return false;
                }
                true
            }
            Err(e) => {
                errors.push(format!("read {}: {e}", path.display()));
                false
            }
        }
    }

    /// Request `textDocument/formatting`, apply returned edits to the active
    /// buffer. Blocks the UI for up to ~1.5s. No-op if no LSP is attached.
    pub(crate) fn format_buffer(&mut self) -> bool {
        self.ensure_lsp_open();
        self.sync_lsp_changes();
        let Some(path) = self.buffer.path() else {
            return false;
        };
        let path: PathBuf = path.to_path_buf();
        let Some(doc) = self.lsp_docs.get(&path).cloned() else {
            return false;
        };
        let Some(client) = self.lsp_clients.get(&doc.server_cmd) else {
            return false;
        };
        let id = client.formatting(doc.uri.clone(), 4, true);
        match client.wait_response(id, Duration::from_millis(1500)) {
            Some((Some(result), None)) => {
                match parse_response::<Vec<vix_lsp::lsp_types::TextEdit>>(Some(result)) {
                    Ok(Some(edits)) => {
                        self.apply_text_edits(&edits);
                        true
                    }
                    Ok(None) => false,
                    Err(e) => {
                        self.msg = format!("lsp format: {e}");
                        false
                    }
                }
            }
            Some((_, Some(err))) => {
                self.msg = format!("lsp format: {err}");
                false
            }
            Some((None, None)) => false,
            None => {
                self.msg = "lsp: format timed out".into();
                false
            }
        }
    }

    /// Format (if LSP is attached) and write to disk.
    pub(crate) fn format_and_save(&mut self) {
        self.format_buffer();
        match self.buffer.save() {
            Ok(()) => {
                self.msg = format!(
                    "\"{}\" written",
                    self.buffer
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                )
            }
            Err(e) => self.msg = format!("error: {e}"),
        }
    }

    /// Apply a slice of LSP `TextEdit`s to the active buffer. Edits are
    /// applied bottom-up (by start position) so earlier offsets stay valid.
    /// Note: `character` is treated as a char index, not UTF-16 code units —
    /// fine for the all-ASCII source files we typically format.
    ///
    /// The whole batch is recorded as a single `Transaction` committed to
    /// `self.history` — so `u` undoes the entire LSP edit in one step. We
    /// deliberately do NOT touch `self.last_change`: LSP edits must not
    /// pollute `.` repeat (Vim's rule; see plan doc trap #2).
    pub fn apply_text_edits(&mut self, edits: &[vix_lsp::lsp_types::TextEdit]) {
        if edits.is_empty() {
            return;
        }
        let sel_before = self.sel;
        let mut tx = Transaction::new();
        tx.sel_before = Some(sel_before);
        apply_text_edits_to_buffer_tx(&mut self.buffer, edits, &mut tx);
        // Cursor may now be past EOF; clamp.
        self.sel = self.sel.clamped(&self.buffer);
        tx.sel_after = Some(self.sel);
        if !tx.is_empty() {
            self.history.commit(tx);
        }
    }
}

/// "E:2 W:1 " if the active buffer has diagnostics, else empty.
pub(crate) fn diag_summary(ed: &Editor) -> String {
    let Some(path) = ed.buffer.path() else {
        return String::new();
    };
    let Some(diags) = ed.diagnostics.get(path) else {
        return String::new();
    };
    let (mut e, mut w) = (0u32, 0u32);
    for d in diags {
        match d.severity.unwrap_or(DiagnosticSeverity::INFORMATION) {
            DiagnosticSeverity::ERROR => e += 1,
            DiagnosticSeverity::WARNING => w += 1,
            _ => {}
        }
    }
    if e == 0 && w == 0 {
        String::new()
    } else {
        format!("E:{e} W:{w} ")
    }
}

pub(crate) fn sev_rank(s: DiagnosticSeverity) -> u8 {
    match s {
        DiagnosticSeverity::ERROR => 4,
        DiagnosticSeverity::WARNING => 3,
        DiagnosticSeverity::INFORMATION => 2,
        DiagnosticSeverity::HINT => 1,
        _ => 0,
    }
}

/// Flatten a hover response to a plain string the TUI can render.
pub(crate) fn hover_text(h: &Hover) -> String {
    let piece = |s: &str| s.to_string();
    match &h.contents {
        HoverContents::Scalar(s) => match s {
            MarkedString::String(s) => piece(s),
            MarkedString::LanguageString(ls) => piece(&ls.value),
        },
        HoverContents::Array(arr) => arr
            .iter()
            .map(|m| match m {
                MarkedString::String(s) => s.clone(),
                MarkedString::LanguageString(ls) => ls.value.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(mc) => mc.value.clone(),
    }
}

/// Escape regex metacharacters for safe interpolation into a search pattern.
/// Apply a slice of LSP `TextEdit`s to an arbitrary buffer. Mirrors
/// `Editor::apply_text_edits` but works on buffers not owned by `Editor`
/// (used when applying rename edits to parked or on-disk buffers).
/// Apply LSP edits to `buf`, optionally recording each primitive edit into
/// `tx` (for history / undo). Pass `&mut Transaction::new()` and discard if
/// you don't want history tracking (e.g. when writing an on-disk file we're
/// not keeping open).
pub(crate) fn apply_text_edits_to_buffer_tx(
    buf: &mut Buffer,
    edits: &[vix_lsp::lsp_types::TextEdit],
    tx: &mut Transaction,
) {
    if edits.is_empty() {
        return;
    }
    let mut sorted: Vec<_> = edits.iter().collect();
    sorted.sort_by(|a, b| {
        let ak = (a.range.start.line, a.range.start.character);
        let bk = (b.range.start.line, b.range.start.character);
        bk.cmp(&ak)
    });
    for e in sorted {
        let start_line = (e.range.start.line as usize).min(buf.len_lines());
        let end_line = (e.range.end.line as usize).min(buf.len_lines());
        let start_char = buf.line_to_char(start_line)
            + (e.range.start.character as usize).min(buf.line_len_chars(start_line));
        let end_char = buf.line_to_char(end_line)
            + (e.range.end.character as usize).min(buf.line_len_chars(end_line));
        if start_char <= end_char && end_char <= buf.len_chars() {
            if start_char < end_char {
                let removed: String = buf.rope().slice(start_char..end_char).to_string();
                buf.remove_range(start_char..end_char);
                tx.push(Change::Delete {
                    at: start_char,
                    removed,
                });
            }
            if !e.new_text.is_empty() {
                buf.insert_str(start_char, &e.new_text);
                tx.push(Change::Insert {
                    at: start_char,
                    text: e.new_text.clone(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vix_core::{Action, Motion, PendingOp, RepeatAction};
    use vix_lsp::lsp_types::{Position, Range, TextEdit};

    fn edit(sl: u32, sc: u32, el: u32, ec: u32, text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: Position {
                    line: sl,
                    character: sc,
                },
                end: Position {
                    line: el,
                    character: ec,
                },
            },
            new_text: text.into(),
        }
    }

    #[test]
    fn lsp_edits_are_undoable() {
        let mut ed = Editor::new(Buffer::from_text("hello world\n"));
        ed.apply_text_edits(&[edit(0, 6, 0, 11, "Rust")]);
        assert_eq!(ed.buffer.rope().to_string(), "hello Rust\n");
        ed.dispatch(Action::Undo);
        assert_eq!(ed.buffer.rope().to_string(), "hello world\n");
    }

    #[test]
    fn lsp_edits_do_not_pollute_dot_repeat() {
        // Set up a dot-repeatable action: `dw` at cursor 0 of "foo bar".
        let mut ed = Editor::new(Buffer::from_text("foo bar\n"));
        ed.dispatch(Action::Operate(PendingOp::Delete, Motion::WordForward, 1));
        assert_eq!(ed.buffer.rope().to_string(), "bar\n");
        let before = ed.last_change.clone();
        assert!(matches!(before, Some(RepeatAction::Operate { .. })));

        // Apply an "LSP-style" edit — should NOT overwrite last_change.
        ed.apply_text_edits(&[edit(0, 0, 0, 0, "baz ")]);
        assert_eq!(ed.buffer.rope().to_string(), "baz bar\n");

        // last_change must still point at the original `dw` action.
        match (&before, &ed.last_change) {
            (
                Some(RepeatAction::Operate { op: op_a, .. }),
                Some(RepeatAction::Operate { op: op_b, .. }),
            ) if op_a == op_b => {}
            _ => panic!("LSP edit polluted last_change: {:?}", ed.last_change),
        }

        // And `.` should still replay the `dw`: from current cursor (start of
        // "baz bar"), `dw` removes "baz ".
        ed.sel = Selection::at(0);
        ed.dispatch(Action::RepeatLastChange);
        assert_eq!(ed.buffer.rope().to_string(), "bar\n");
    }

    #[test]
    fn lsp_edit_then_undo_preserves_dot_repeat() {
        let mut ed = Editor::new(Buffer::from_text("foo bar\n"));
        ed.dispatch(Action::Operate(PendingOp::Delete, Motion::WordForward, 1));
        ed.apply_text_edits(&[edit(0, 0, 0, 0, "baz ")]);
        // Undo the LSP edit.
        ed.dispatch(Action::Undo);
        assert_eq!(ed.buffer.rope().to_string(), "bar\n");
        // Dot-repeat is still the original dw.
        assert!(matches!(ed.last_change, Some(RepeatAction::Operate { .. })));
    }
}
