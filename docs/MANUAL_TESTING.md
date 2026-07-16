# Manual Testing Playbook

The integration suite in `crates/tui/tests/` covers buffer/cursor/mode state for
every motion, operator, text object, ex command, register, undo, jump list,
visual mode, dot-repeat, and language detection path. What it **cannot** reach
without a real terminal and live external processes:

- Terminal rendering (colors, cursor shape, gutter alignment, statusline)
- Live LSP servers (rust-analyzer, pyright, tsserver, gopls)
- OSC 52 clipboard escape sequences
- File watchers / format-on-save round-trips that touch disk
- Picker UX latency and incremental match feel
- Tree-sitter highlight visual correctness
- Terminal resize, suspend/resume, mouse focus events

This doc is the playbook for those. Run through the relevant section before
cutting a release or after touching anything in the corresponding subsystem.

## Setup

Build a debug binary once:

```sh
cargo build
BIN=./target/debug/vix
```

Or release for performance-sensitive checks (highlight on big files, picker
latency):

```sh
cargo build --release
BIN=./target/release/vix
```

Fixtures live at `tests/fixtures/`:

- `sample.rs` / `sample.py` / `sample.ts` / `sample.md` / `sample.json`
- `project/` — a tiny multi-file Rust workspace for picker + LSP testing

For LSP checks you need `rust-analyzer`, `pyright`, `typescript-language-server`,
and `gopls` on `$PATH`. Install whatever you have:

```sh
brew install rust-analyzer pyright typescript-language-server
```

---

## 1. Rendering & TUI shell

Goal: confirm the editor draws correctly across terminal sizes and key states.

```sh
$BIN tests/fixtures/sample.rs
```

- [ ] Statusline shows mode, file name, dirty `[+]` flag, line/col.
- [ ] Gutter shows absolute line numbers; current line highlighted.
- [ ] Cursor shape changes per mode (block in Normal, bar in Insert, underline
      in Replace if/when added).
- [ ] Resize the terminal smaller and larger — content reflows, no panics, no
      stale rows.
- [ ] `:set number!` / `:set relativenumber!` toggles take effect immediately.
- [ ] Typing `:` shows a colon prompt at the bottom; backspace at empty cmdline
      exits Command mode.
- [ ] `/foo<CR>` highlights matches; `:noh` clears them.
- [ ] Long lines wrap or scroll consistently with the configured behavior.
- [ ] Suspend with `Ctrl-Z`, resume with `fg` — terminal returns to a clean
      state (no leftover alt-screen artifacts).

## 2. Tree-sitter highlighting

```sh
$BIN tests/fixtures/sample.rs
$BIN tests/fixtures/sample.py
$BIN tests/fixtures/sample.ts
$BIN tests/fixtures/sample.md
$BIN tests/fixtures/sample.json
```

- [ ] Each file is colorized; keywords, strings, comments, and types are
      visually distinct.
- [ ] Edit a token (`iX<Esc>`) — highlight updates within one frame, no
      stale spans.
- [ ] Open a 100k-line file (e.g. concatenate `sample.rs` 5000×) — typing stays
      responsive, scroll is smooth.
- [ ] `notes.unknownext` opens with no highlight and no error.

Quick big-file generator:

```sh
for i in $(seq 1 5000); do cat tests/fixtures/sample.rs; done > /tmp/big.rs
$BIN /tmp/big.rs
```

## 3. Pickers

```sh
$BIN tests/fixtures/project/src/main.rs
```

- [ ] `Ctrl-P` / `<Space>f` / `:Files` opens the omnibox: a centered,
      bordered input box with an edge-to-edge result list under it. It's
      single-mode — you're always typing, no separate nav/input modes. `j`/
      `k`/`Ctrl-j`/`Ctrl-k`/arrows move the selection while typing continues
      to filter.
- [ ] With an empty query, the list shows this project's recently-opened
      files (most-recent first), not every file in the tree.
- [ ] Typing a query blends fuzzy file-name hits and (once you type
      `MIN_CONTENT_QUERY_LEN`+ chars) literal smart-case content hits into
      one ranked list — file-name hits sort above content hits.
- [ ] `<Tab>` cycles the filter indicator All → Files → Content → All (shown
      on the input box's top border), preserving the query.
- [ ] `<Esc>` closes the omnibox immediately. If it's the launch omnibox
      (`vix` with no file argument) and nothing has been picked yet, `<Esc>`
      quits vix instead of leaving an empty placeholder buffer.
- [ ] `:Buffers` lists the active + parked buffers in vix's own fullscreen
      split-pane layout (list + preview pane, ≥ 80 cols); selecting one
      switches to it without disturbing the unnamed register.
- [ ] `:Grep foo` / `<Space>g` opens the omnibox pre-filtered to Content with
      the query pre-filled; entries show `file:line` and a snippet.
- [ ] `:Symbols` on a Rust file shows top-level fns/structs/impls; on
      `notes.unknownext` it surfaces the "no language" message and refuses to
      open.
- [ ] Omnibox honours `.gitignore` (drop a `node_modules/` or `target/` and
      verify they're skipped) for both the file-name and content sources.

## 4. LSP — rust-analyzer

```sh
$BIN tests/fixtures/project/src/main.rs
```

Wait a few seconds for indexing.

- [ ] Statusline shows the LSP is attached / indexing / ready.
- [ ] Introduce a syntax error (`let x =<Esc>`) — diagnostic appears in the
      gutter and statusline within a couple seconds.
- [ ] Hover on an identifier shows type / docs.
- [ ] `gd` jumps to definition; jump list records the source position
      (verify with `Ctrl-O` to return).
- [ ] Trigger completion (e.g. `String::` then wait) — popup appears, arrow
      keys navigate, `<Tab>`/`<CR>` accepts, `<Esc>` cancels.
- [ ] `:Rename newname<CR>` on a symbol renames every occurrence across the
      project (check sibling files in `tests/fixtures/project/src/`).
- [ ] Code actions menu lists at least one action on a fixable diagnostic;
      applying it edits the buffer and is undoable with `u`.
- [ ] `:w` triggers format-on-save; the buffer reflows and no pending edits are
      lost.
- [ ] Kill rust-analyzer externally (`pkill rust-analyzer`) — vix detects it,
      shows a status message, and auto-restarts on the next edit.

## 5. LSP — other servers

Repeat the relevant subset of (4) for:

- [ ] `pyright` on `tests/fixtures/sample.py`
- [ ] `typescript-language-server` on `tests/fixtures/sample.ts`
- [ ] `gopls` on a `.go` file you have lying around

For each: diagnostics, hover, goto-definition, completion, format-on-save.

## 6. Dot-repeat across LSP edits

The integration suite verifies LSP edits don't pollute `.`. Sanity-check live:

- [ ] `dw` then `.` — repeats the delete.
- [ ] Apply a code action / rename / format-on-save in between — `.` still
      replays the last manual change, not the LSP edit.
- [ ] `u` undoes the LSP edit cleanly.

## 7. OSC 52 clipboard

OSC 52 cannot be observed by the harness because it's a terminal escape
sequence. Test in a real terminal:

- [ ] In a local terminal (Terminal.app, iTerm2, Alacritty, kitty, wezterm),
      yank with `"+yy`. Paste into another app — it pastes.
- [ ] Over SSH from a remote host into a local terminal, `"+yy` still pastes
      locally (OSC 52 should pass through).
- [ ] Inside tmux/zellij, OSC 52 requires the multiplexer to forward it. tmux
      needs `set -g set-clipboard on`; zellij forwards by default in recent
      versions. Note the result for each.
- [ ] Yanking a multi-line region works; trailing newline preserved.
- [ ] Plain `yy` does **not** touch the system clipboard, only the unnamed
      register.

## 8. File I/O & disk side effects

```sh
cp tests/fixtures/sample.rs /tmp/scratch.rs
$BIN /tmp/scratch.rs
```

- [ ] `:w` writes to disk; `mtime` updates; content matches buffer.
- [ ] `:w /tmp/other.rs` writes to a new path without changing the buffer's
      backing path.
- [ ] `:wq` writes and exits with status 0.
- [ ] `:q` on a dirty buffer is blocked with a message; `:q!` discards.
- [ ] `:e other.rs` parks the current buffer; `:e!` reloads from disk and
      drops local edits.
- [ ] Open a read-only file (`chmod 444`); `:w` reports the error without
      crashing.
- [ ] Open a path that doesn't exist; the buffer opens empty with the path
      attached; `:w` creates the file.

## 9. Multi-buffer flow

- [ ] `vix a b c` opens with `a` active and `b`, `c` parked.
- [ ] `:ls` lists all three with the active one marked.
- [ ] `:bn` / `:bp` cycles through them in vix's documented order
      (active=1, parked oldest-first).
- [ ] `:b 2` jumps to buffer 2.
- [ ] `:bd` closes the active buffer; the next parked one becomes active.
- [ ] Edits in a parked buffer survive a switch-away and switch-back (vix's
      `hidden` semantics — parked buffers can be dirty).

## 10. Search UX

- [ ] `/foo` highlights all matches as you type (live).
- [ ] `<CR>` jumps to the next match; `<Esc>` cancels and leaves cursor put.
- [ ] `n` / `N` step through; `*` / `#` search word under cursor.
- [ ] Smart-case: `/foo` matches `Foo`; `/Foo` does not match `foo`.
- [ ] `:noh` clears highlight without changing cursor.
- [ ] `:%s/foo/bar/g` substitutes across the file; `c` flag prompts per match.

## 11. Crash & recovery

- [ ] `kill -9 $(pgrep vix)` while editing — restarting on the same file does
      not lose work if you previously `:w`'d. (Vix has no swap file; this is
      mainly to confirm clean termination behavior.)
- [ ] Open a binary file (`/bin/ls`) — vix should refuse or show a clear
      message, not garble the terminal.
- [ ] Open a file with mixed line endings — display is sane, `:w` round-trips
      without converting silently.

## 12. Release-binary smoke

For every release tarball:

```sh
tar xf vix-<target>.tar.gz
./vix --version
./vix tests/fixtures/sample.rs
```

- [ ] Linux musl: `file ./vix` reports static, no dynamic deps.
- [ ] macOS universal: `lipo -info ./vix` lists both `x86_64` and `arm64`.
- [ ] Binary size is in the documented range (15–25 MB stripped).
- [ ] Runs on a clean machine with no Rust toolchain installed.
- [ ] No network calls on first launch (`sudo lsof -i -p $(pgrep vix)` shows
      none beyond the LSP server connections you opted into).

---

## Reporting

When you find a regression here, file it with:

1. The section + checkbox that failed.
2. The fixture (or `/tmp/...` content) used.
3. Terminal name + version, multiplexer (if any), OS.
4. Exact key sequence to reproduce.

Add a corresponding integration test under `crates/tui/tests/` if the failure
is one the harness *could* have caught — that's the cheapest way to make sure
it doesn't come back.
