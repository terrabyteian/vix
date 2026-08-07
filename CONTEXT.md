# vix — context

A modal, vim-flavored terminal editor. Zero-config, single binary, Unix/macOS
only. Cargo workspace: `crates/core` (editing engine), `crates/tui` (ratatui
frontend), `crates/syntax` (tree-sitter highlighting), `crates/picker` (fuzzy
match + file walk + grep), `crates/lsp` (JSON-RPC client), `crates/app` (the
`vix` binary).

This file holds the project's vocabulary and its hard constraints. Decisions
with a rationale worth preserving live in `docs/adr/`. What shipped when lives
in git history.

## Glossary

Use these terms as defined here — in issue titles, test names, commit
messages, and code. Where two words could mean the same thing, only the one
listed is current.

**Buffer** — one open file (`core::buffer`). Wraps a ropey `Rope` and owns the
path, dirty flag, and a monotonic `version()` counter bumped on every
structural mutation. Caches (syntax spans, LSP doc sync, markdown layout) key
off that version rather than diffing content.

**Scratch buffer** — a buffer with no on-disk backing (`:help` output). `:w`
against one returns `BufferError::Scratch` rather than writing a file named
after the synthetic path.

**Active buffer / parked buffer** — exactly one buffer is *active*, held as
inline fields on `Editor`. The rest are *parked* as `BufferSave` snapshots
(`tui::buffers`) carrying their own history, selection, view state, and view
mode. `save_active` / `install_active` swap between the two. Vim `hidden`
semantics: a parked buffer may be dirty.

**Selection** — a single cursor as `anchor` / `head` / `virt_col`
(`core::selection`). There is exactly one; see the cursor-first commitment
below.

**Mode** — `Normal` / `Insert` / `Visual` / `VisualLine` / `Command`, plus
`PendingOp` for operator-pending state (`core::mode`).

**Repeat intent** — what `.` replays, recorded as a `RepeatAction`
(`core::edit`) at the resolved-dispatch level: the *operation* that was
performed, not the keys that triggered it and not the resulting diff.

**Picker** — the overlay UI for choosing something (`tui::picker`). One
implementation, several `PickerKind`s: `Omni`, `Symbols`, `Buffers`,
`CodeActions`, `Jumps`.

**Omnibox** — the `PickerKind::Omni` picker specifically: one query blending
two streamed sources (file names and file contents) into a single ranked list,
`<Tab>` cycling the source filter, a leading `/` switching to regex
content-only search. "Omnibox" names this picker's kind and its UI; it is not a
separate subsystem from the picker.

**Project view** — the omnibox's empty-query state: this project's recent
files above a path-ordered listing of every file in the project. Reached at
launch (`vix` with no path, or with a directory) and any time the omnibox is
open with nothing typed. Named for its job — seeing the project — as against
the recents-only view it replaces.

**View mode** — per-buffer `Raw` or `Rendered` (`tui::markdown::ViewMode`).
Markdown opens `Rendered` by default; `Space m` toggles. The mode is parked and
restored with the buffer.

**Harness** — the headless test driver (`tui::testing`). Drives a real `Editor`
via `handle_key` with a vim-flavored key DSL. Integration tests go through it
rather than constructing editor state directly.

## Architectural commitments

Changing any of these is a redesign, not a refactor. If a change requires one,
say so explicitly rather than drifting.

- **Cursor-first, not selection-first.** vim's model, not Helix's: a motion
  moves a cursor, and an operator consumes a motion. Not "select then act."
- **Repeat intent is recorded at the resolved/dispatched level** — not raw
  keys (which can't survive remapping or counts) and not raw diffs (which
  can't replay against different text).
- **An insert-mode session is one undo unit.** Entering insert opens a
  transaction; `Esc` commits it.
- **Char offsets are the canonical internal cursor position.** Convert to
  bytes only at the tree-sitter and LSP boundaries.
- **ropey 1.6, not 2.0-beta.**

## Deliberate non-goals

These are decided, not deferred. Don't implement them, and don't open issues
proposing them without new information.

- Windows support
- A plugin system
- A user config file (no `~/.config/vix/*`) — zero-config is the point
- Multi-cursor
- Bundled LSP binaries
- Splits/windows — use `:Buffers`, `:bn`/`:bp`, and terminal multiplexer panes
- A persistent file-tree pane or sidebar — the Project view covers seeing
  project structure; a docked tree is the splits/windows non-goal wearing a
  different hat
- Visual block mode — `:%s` and multi-line insert cover the real cases
- Macros (`q`/`@`) — `.` repeat covers the common uses
- Marks (`m`/`'`) and the change list (`g;`/`g,`) — the jump list subsumes both
- Folding, spell check
- Named registers beyond `"` and `"+`
