# vix — Phase Progress

Single source of truth for implementation state. Keep this in sync at the end of each work session so any future session (human or AI) can pick up cold.

Plan: `~/.claude/plans/1-rough-estimates-of-rippling-dolphin.md`

## Phase 1 — Core editor — COMPLETE ✓

### Delivered
- Cargo workspace: `crates/core`, `crates/tui`, `crates/app`
- `core::buffer` — Ropey wrapper, line/char conversions, load/save, dirty flag
- `core::selection` — single-cursor Selection with anchor/head/virt_col
- `core::mode` — Normal/Insert/Visual/VisualLine/Command + PendingOp
- `core::motion` — hjkl, w/b/e, 0/^/$, gg/G with `<n>G`/`<n>gg` line-jump, f/F/t/T, `%` bracket-match, counts on all
- `core::keymap` — operator-pending FSM, counts (`3w`, `2d3w`), g-prefix (gg, gu, gU), text-object prefix (`i`/`a`), find-target consumption
- `core::textobject` — iw/aw, i"/a"/i'/a'/i\`/a\`, i(/a(, i{/a{, i[/a[, i</a>, + `b`/`B` aliases
- `core::edit` — Transaction (Insert/Delete changes), History (undo/redo stacks with redo-clear-on-commit), RepeatAction (dot)
- `core::search` — regex forward/backward/find_all_in_lines with smart-case
- TUI: ratatui render with gutter+statusline+cmdline, mode-colored status, cursor overlay, viewport scrolling, yellow search highlight, blue visual-selection highlight (layered)
- Insert mode: `i/I/a/A/o/O` + Esc; Backspace/Tab records; session-scoped Transaction commits on Esc → one undo unit
- Normal operators: `d`, `c`, `y` (+ unnamed register), `>`/`<` indent (4-space), `~` toggle case, `gu`/`gU` case ops, all via motion/text-object/linewise
- Visual mode: motions extend head, `d`/`c`/`y`/`~`/`>`/`<` operate on selection and return to Normal; `v`/`V` toggle between charwise/linewise
- Paste: `p`/`P` with linewise vs charwise semantics from the register
- Ex commands: `:w`, `:q`, `:q!`, `:wq`, `:x`, `:noh`
- Ex substitute: `:%s/pat/rep/flags` (`g`, `i` flags), `:.s/...`, `:s/...` — single-transaction undo
- Search: `/`, `?`, live; `n`/`N` repeat with wrap-around; `*`/`#` word-under-cursor
- Undo/redo: `u`, `Ctrl-R`, `.` repeats last Operate/OperateLine/OperateObject/InsertBurst

### Stats
- 46 unit tests, all passing (`cargo test -p vix-core`)
- Clippy clean (`cargo clippy --workspace --all-targets`)
- Release binary: **1.4 MB** (regex pulls in aho-corasick)

### Dogfood commands that should just work
```
cargo run --release -- crates/core/src/motion.rs
```
In the editor:
- `3dw`, `diw`, `ci"`, `yy`, `p`, `P`
- `dfX`, `ct)`, `;`, `,`
- `/foo<CR>`, `n`, `N`, `*`, `#`
- `:%s/foo/bar/g`, `:noh`
- `u`, `<C-r>`, `.`
- `v` then `iw` then `d`
- `5G`, `gg`, `%`
- `guiw`, `gUU`

## Phase 2 — Syntax + pickers — COMPLETE ✓

### Delivered
- `crates/syntax` scaffolded with `tree-sitter 0.25` + `tree-sitter-highlight`
- `Language` enum with 12 variants: Rust, Python, JavaScript, TypeScript, Tsx, Go, Markdown, Json, Toml, Html, Css, Bash
- `Language::from_path` detects by extension (+ `.bashrc`-style filenames)
- `SyntaxState::new(lang)` constructs a `HighlightConfiguration` per language, using each grammar crate's native API (JS uses `HIGHLIGHT_QUERY` singular, Bash uses `HIGHLIGHT_QUERY`, Markdown uses block language + `HIGHLIGHT_QUERY_BLOCK`, TS/TSX share `HIGHLIGHTS_QUERY` via two `LANGUAGE_*` consts)
- `SyntaxState::highlight(src)` returns `Vec<HlSpan { range, scope }>` via `tree_sitter_highlight::Highlighter` — flattens nested scopes to innermost
- TUI integration: Editor holds `Option<SyntaxState>`, computed from buffer path at `Editor::new`. Full-buffer reparse per frame (no incremental yet). Spans applied as base style layer in `render_content`; search/visual/cursor overlays win when present
- Scope → Color mapping in `scope_style`: keyword→Magenta, function→LightBlue, type→Cyan, string→LightYellow, constant→LightRed, comment→DarkGray, attribute/constructor→LightMagenta, namespace/label→Yellow, property→LightCyan, tag→LightGreen. Punctuation/operator/variable stay default

### Pickers (delivered)
- `crates/picker` scaffolded with `nucleo-matcher`, `ignore`, `grep-regex`, `grep-searcher`
- `scan_files(root)` walks the tree respecting `.gitignore`
- `grep(root, pattern)` returns per-line matches via `grep-searcher`
- `score(items, query, limit)` runs the nucleo smart-case fuzzy matcher and returns top-N by score
- `Utf32String` re-exported from `nucleo-matcher` so downstream crates don't need the dep
- TUI overlay: centered box, query prompt + scrollable match list, selection highlight, match-count indicator
- Input routing: when `picker` is Some, intercepts all keys. Esc closes, Enter selects, Up/Down + Ctrl-N/Ctrl-P navigate, Backspace edits query, printable chars append
- Ex commands: `:Files` opens file finder; `:Grep <pattern>` opens grep picker; `:e <path>` loads a file directly
- Selection actions: `:Files` → `open_path(path)` replaces current buffer (refuses if dirty, mirrors `:e`); `:Grep` hit → open + jump to line
- Loading a file auto-rebinds `SyntaxState` from the new extension

### Multi-buffer + `:Buffers` picker (delivered)
- `BufferSave` struct parks a full per-buffer state snapshot (buffer, sel, history, view_top, syntax state/cache/version, pending_insert, last_change)
- Active buffer stays as inline fields on `Editor` (zero refactor of the ~200 existing field-access sites); parked buffers live in `other_buffers: Vec<BufferSave>`
- `save_active` / `install_active` do the swap; `switch_to_buffer(idx)` + `add_or_switch_buffer(buf)` are the public entry points. `:e` / `:Files` / `:Grep` selection now *adds* a new buffer instead of clobbering, and switches if the path is already open
- Ex commands: `:Buffers` / `:ls` open the picker. `:b <n>` / `:b <substring>`. `:bn` / `:bp` cycle. `:bd` / `:bd!` close. `:q` closes just the current buffer (quits editor when it's the last one). `:qa` / `:qa!` quit everything
- Statusline shows `[1/N]` when multiple buffers are open
- Vim `hidden` semantics — parked buffers may be dirty; `:qa` refuses to quit until they're all saved (or use `:qa!`)

### Symbol picker (delivered)
- `SyntaxState::symbols(src) -> Vec<Symbol>` parses with a fresh `tree_sitter::Parser` and runs a hand-written `Query` per language
- Per-language support: Rust (fn/struct/enum/trait/const/static/macro/mod), Python (fn/class), JS (fn/class/method), TS+TSX (fn/class/interface/type/method), Go (fn/method/type)
- Unsupported languages return an empty vec — picker surfaces "no symbols found"
- TUI: `:Symbols` opens picker with `kind  name  Lline` display; selection jumps cursor to the symbol's char offset. Reuses the picker overlay
- New `PickerValue::BufferOffset(usize)` for in-buffer jumps — generalizes to any cursor-jump picker we add later

### Syntax span caching (delivered)
- `Buffer::version()` — monotonic counter bumped on every structural mutation (`insert_char`, `insert_str`, `remove_range`, `rope_mut`)
- Editor caches `(syntax_cache: Vec<HlSpan>, syntax_version: Option<u64>)`. `refresh_syntax_cache()` compares versions before re-parsing; pure cursor movement / scrolling pays **zero parse cost**
- `invalidate_syntax_cache()` called when the buffer is swapped via `:e` / `:Files` (new buffer's version counter starts at 0, which could collide with a stale cache)
- Not true incremental reparse (`tree-sitter-highlight` doesn't let us hand it an old `Tree`), but eliminates the common wasted-work path

### Stats
- 58 unit tests passing (48 core + 8 syntax + 2 picker)
- Clippy clean
- Release binary: **1.4 MB → 8.5 MB** (6.7 MB for 11 bundled grammars, ~0.4 MB for picker deps; well under the 15-25 MB plan budget)

### Deferred from Phase 2 (not blocking)
- True incremental reparse (byte-edit deltas via `tree_sitter::InputEdit`) — `tree-sitter-highlight` doesn't expose the old tree, so would require hand-rolled parse+query instead of using the `Highlighter` API. Version-based cache avoids the pain point (wasted parse on pure navigation); only matters on very large files + rapid edits

### File map (new)
- `crates/syntax/src/lib.rs` — Language detection + highlighting
- `crates/syntax/Cargo.toml` — Grammar deps
- `crates/picker/src/lib.rs` — File walk + grep + nucleo scoring
- `crates/picker/Cargo.toml` — Picker deps
- `crates/tui/src/lib.rs` — `scope_style`, `compute_syntax_spans`, syntax overlay, Picker struct + overlay + input routing

## Phase 3 — LSP core — COMPLETE ✓

### Delivered
- `crates/lsp` scaffolded: hand-rolled JSON-RPC over `lsp-types` + `tokio` + `serde_json`. Uses lsp-types 0.97 (`Uri` type, not `Url`)
- `LspClient::start(config, root)` spawns the server binary, does the `initialize`/`initialized` handshake synchronously, and returns a handle the sync TUI can poll. Runtime architecture: one dedicated OS thread per client hosting a `tokio::runtime::Builder::new_current_thread()` runtime; outbound commands via `tokio::sync::mpsc`, inbound events via `std::sync::mpsc`. Avoids rewriting the crossterm event loop as async
- Public surface: `did_open` / `did_change_full` / `did_close` / `hover` / `definition` / `shutdown` / `try_recv`. Every request returns a `RequestId` the caller correlates against `ServerEvent::Response`. `parse_response::<T>` is a typed helper
- Server registry via `ServerConfig::{rust_analyzer,pyright,typescript,gopls}` + `server_for_path(&Path)`. `config.available()` checks `$PATH` for the binary; missing-binary failures are cached in `lsp_failed` so we don't spam respawn attempts
- `path_to_uri` / `uri_to_path` — hand-rolled percent-encoding so we can round-trip `file://` URIs between the editor (`PathBuf`) and the server (`Uri`)
- Framing parser uses `read_line` + `trim_end()` for the `\r\n\r\n` terminator. **Initial bug:** chaining `trim_end_matches('\r').trim_end_matches('\n')` leaves a trailing `\r` because the chars come in wrong order — causes the blank-line check to fail forever and the header loop hangs silently. Use `trim_end()` which strips all ASCII whitespace

### TUI integration (delivered)
- `Editor` owns `lsp_clients: HashMap<String, LspClient>` (keyed by server cmd, one per language), `lsp_docs: HashMap<PathBuf, LspDocState>` (per-buffer LSP doc state — URI, version, last-sent buffer version, owning server cmd), `diagnostics: HashMap<PathBuf, Vec<Diagnostic>>`, `pending_requests: HashMap<(String, RequestId), PendingRequest>`, `lsp_failed: HashSet<String>`
- `ensure_lsp_open` spawns the server (lazy, per-language) and sends `didOpen` for the active buffer. Called from `Editor::new`, `add_or_switch_buffer`, and both `request_hover`/`request_definition` as a safety net
- `sync_lsp_changes` full-text `didChange` batching: compares `Buffer::version()` against `LspDocState.last_sent_buffer_version` and pushes a whole-buffer `didChange` on divergence. Called from `render` so every frame starts with the server in sync. Full-text instead of incremental — simple, correct, good enough until we profile
- `drain_lsp_events` pulls pending events from every client each tick. The run loop uses `event::poll(Duration::from_millis(100))` so LSP notifications flow even with no keystrokes
- Gutter shows a per-line severity dot: red ● error, yellow ● warning, cyan ● info, gray ○ hint. Statusline has `E:n W:n` counts for the active buffer
- `K` → hover. Response rendered as a dismissable popup in the top-right (markdown/markedstring flattened to plain text; word-wrapped to 2/3 of screen width). Any keypress closes it
- `gd` → goto-definition. Single Location / first of Array / first of Link → `open_path` + jump. Added as new `Action::LspHover` and `Action::LspGotoDefinition` in core — `K` at Normal top-level; `gd` via the existing `g`-prefix state machine

### Stats
- 61 tests passing (48 core + 8 syntax + 2 picker + 2 lsp-unit + 1 lsp-smoke [spawns real rust-analyzer on `/tmp`, verifies a real diagnostic arrives — 1.7s])
- Clippy clean (`-D warnings`)
- Release binary: **8.5 MB → 8.7 MB** (~200 KB for lsp-types + serde_json + tokio process feature)

### Deferred from Phase 3 (not blocking)
- Incremental `didChange` (currently full-text; tolerable for the 1–10k-line files people edit interactively)
- LSP auto-restart on server crash (we detect `ServerEvent::Exited` but don't respawn)
- Completion popup (kept for Phase 4 — needs a proper floating widget that tracks cursor)
- Code actions / rename / format-on-save (Phase 4)
- `workspace/symbol` wiring into the existing Symbols picker as an alt source (Phase 4)
- Server→client request handling (`workspace/configuration`, `client/registerCapability`) — we ignore these; rust-analyzer tolerates it

## Phase 4 — Completeness + polish (in progress)

### Delivered so far
- **LSP completion popup** in Insert mode. `Ctrl-Space` primary trigger (zellij-safe); `Ctrl-N`/`Ctrl-P` as fallback. Navigate Up/Down/Tab/Enter. Filters live as you type; dismisses if prefix no longer matches or cursor moves before `prefix_start`.
- **Format-on-save**. `:w` (and `:wq`/`:x`) sends `textDocument/formatting`, waits up to 1.5s, applies edits, writes. `:fmt` formats without saving.
- **OSC 52 system clipboard** on yank. `y`/`yy`/`Y` copies via OSC 52 escape (works over SSH). Hand-rolled base64, no extra dep. 100KB cap.
- **Rename via `:rename <new>`**. `textDocument/rename`, applies `WorkspaceEdit` (both legacy `changes` and modern `documentChanges`) across active + parked buffers + on-disk files (non-open files are loaded, edited, written back). Reports `renamed in N file(s), M edit(s)`.
- **Code actions via `gA` or `:action`**. Requests `textDocument/codeAction` scoped to cursor (or Visual selection), passes diagnostics on current line as context, shows a nucleo-fuzzy picker of titles, applies the chosen action's edit + command on accept.
- **LSP auto-restart**. When a server crashes (`ServerEvent::Exited`), the dead client is torn down and respawned on the next edit. Rate-limited: 3 restarts per 60s before giving up.
- **Yank flash indicator**. Yanked range highlights yellow for ~150ms so the op is visible.
- **Path absolutization** in `Buffer::load` — relative paths work with LSP URIs.
- **`x` / `X` char-delete** — dedicated action clamped to line end, so it deletes the final char on a line (old `dl`/`dh` path failed there).
- **LSP blocking request helper** (`LspClient::wait_response`) with internal holding queue so non-matching events are preserved for the TUI's `try_recv`.

### Still to do
- Splits / windows (`:split`, `:vsplit`, `Ctrl-W hjkl`) — deferred: needs a proper Window struct + LayoutNode tree + focus routing for ~114 call sites that currently assume a single active view. A dedicated session.
- `=` indent via tree-sitter indent queries
- `.` repeat hardening across LSP-driven edits
- Visual block mode (maybe)

## Phase 5 — Ship
Not started. Static musl linux builds, macOS universal binary, Homebrew tap, GitHub releases.

## Deliberate non-goals (do NOT implement)
- Windows support (Unix/macOS only)
- Plugin system
- User config file (no `~/.config/vix/*`)
- Multi-cursor
- Bundled LSP binaries

## Architectural commitments (don't drift)
- Cursor-first, NOT selection-first like Helix
- Record repeat intent at resolved/dispatched level (not raw keys, not raw diffs)
- Insert-mode session = one undo unit
- Char offsets as canonical internal cursor position (convert to bytes at tree-sitter/LSP boundaries later)
- ropey 1.6 (stable), NOT 2.0-beta

## Known rough edges / tech debt
- `~` moves cursor to `new_len - 1` — subtle off-by-one to revisit
- `.` repeat for `c<motion>...<Esc>` currently records the delete and insert as two separate repeatables; should fuse to a single ChangeBurst (noted in `dispatch`)
- Indent operator applies 4 spaces flat; no tab-respect or file-configured shiftwidth — fine until Phase 4
- Visual mode cursor rendering: in VisualLine mode the cursor position may look odd near line ends
- No yanked-range flash indicator
- `:s` doesn't support backreferences in replacement (`$1`, `\1`); just literal text and `&` is not translated — only plain replacement for now

## File map (critical files)
- `crates/core/src/buffer.rs`
- `crates/core/src/selection.rs`
- `crates/core/src/mode.rs`
- `crates/core/src/motion.rs`
- `crates/core/src/keymap.rs`
- `crates/core/src/textobject.rs`
- `crates/core/src/edit.rs`
- `crates/core/src/search.rs`
- `crates/syntax/src/lib.rs` — tree-sitter integration, language detection, highlighting
- `crates/picker/src/lib.rs` — nucleo fuzzy + ignore walk + grep
- `crates/lsp/src/lib.rs` — JSON-RPC client, server registry, URI helpers
- `crates/lsp/tests/smoke.rs` — real-rust-analyzer roundtrip test (gated on `$PATH`)
- `crates/tui/src/lib.rs` — Editor struct, dispatch, render, scope→color mapping, picker overlay, LSP bridging (ensure_lsp_open/sync_lsp_changes/drain_lsp_events)
- `crates/app/src/main.rs` — entrypoint

## Next session starter
```
cd ~/projects/vix
cat PROGRESS.md                                      # orient
cargo test --workspace                                # should be all green
cargo clippy --workspace --all-targets                # should be clean
cargo run --release -- crates/core/src/motion.rs     # dogfood
```
Phase 4 is partially delivered (completion, format-on-save, OSC 52, rename). Remaining: code actions, splits, indent queries, `.`-repeat hardening, visual block, crash recovery.

Phase 2 dogfood commands:
- `:Files` — fuzzy file finder (respects `.gitignore`)
- `:Grep <pattern>` — project-wide regex search, jump to hit
- `:Symbols` — tree-sitter outline for current buffer (Rust/Python/JS/TS/Go)
- `:Buffers` / `:ls` — open buffer picker; `:bn` / `:bp` cycle; `:bd` close
- `:b <n>` or `:b <substring>` — jump to buffer by number or path fragment

Phase 3 dogfood commands (requires the server binary on `$PATH` — `rust-analyzer`, `pyright-langserver`, `typescript-language-server`, or `gopls`):
- Open a .rs/.py/.ts/.go file — server spawns lazily; diagnostics appear in the gutter + statusline within a few seconds
- `K` — hover at cursor (popup top-right; any key dismisses)
- `gd` — goto-definition; loads the target file in a new buffer if needed
