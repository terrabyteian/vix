# Coding standards

What this repo actually does. These are descriptive — they were extracted from
the tree, not aspirational. A change that departs from one should say why.

`/code-review`'s Standards axis reviews against this file.

## Gates

Nothing lands red:

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Clippy is zero-warning, not warning-tolerant. `cargo fmt` output is
authoritative — no hand-formatting against it.

The `vix-lsp` smoke test spawns a real rust-analyzer and skips itself (still
green) when one isn't runnable. A skip is not a failure; a *hang* is.

## Testing

**Unit tests are inline**, in a `mod tests` at the bottom of the file they
test. This holds across the workspace — `core/src/motion.rs`,
`syntax/src/lib.rs`, `tui/src/markdown.rs`, `tui/src/picker/mod.rs` all follow
it. Don't add a `tests/` file for something that could be a `mod tests`.

**Integration tests go through `Harness`** (`crates/tui/src/testing.rs`), one
file per feature area under `crates/tui/tests/` — `motions.rs`, `operators.rs`,
`markdown_view.rs`, and so on. Add to the matching file; create a new one only
for a genuinely new area.

`Harness` drives a real `Editor` through `handle_key` with a vim-flavored key
DSL (`<Esc>`, `<CR>`, `<C-x>`, `<lt>`). Assert on the accessors — `text()`,
`cursor()`, `mode()`, `register()`, `jump_list()`, `picker_open()` — not on
editor internals. **Do not construct editor state directly to reach a case**;
if a state isn't reachable through keys, that's a finding about the design, not
a reason to bypass the harness.

`Harness::hermetic` strips real-world side effects at construction (currently
the recent-files store). Anything new that touches user data or the network at
`Editor::new` must be neutralized there too — tests never write to a real
user's data.

**Tests never touch process-global state.** The process cwd is the one that
keeps coming up: a test that `chdir`s serializes the whole binary and makes
`cargo test --workspace` intermittently wrong for anyone who doesn't know to
pass `--test-threads=1`. A test that needs a fixture repo builds a temp dir and
calls `Harness::set_root` — the project root is `Editor` state (see
`CONTEXT.md`), not process state. If new code needs ambient context, thread it
through the editor the same way rather than reading it back out of the process.

**Render-path behavior is not harness-testable.** The suite asserts on state,
not drawn cells. Statusline chips, highlight colors, and cursor placement need
either a unit test on the layout function or a `docs/MANUAL_TESTING.md` entry —
say which in the PR rather than leaving it silently uncovered.

`unwrap()` / `expect()` are fine in tests and rare in production code. Keep it
that way: production paths surface failure rather than panicking.

## Errors

Three layers, don't mix them:

- **Library crates** define a `thiserror` enum (`BufferError` in
  `core/src/buffer.rs` is the model). One variant per distinguishable failure,
  with a lowercase message.
- **`anyhow`** is available for the binary and for glue where the caller can't
  act on the variant.
- **User-facing failure sets `self.msg`** — the statusline. ~90 sites do this.
  Messages are lowercase, terse, and say what happened, not what the code was
  doing. Where vim has an error code, use it (`E471: usage :s/pat/rep/flags`).

Don't `eprintln!` from the TUI — the terminal is in raw mode and the message is
lost or corrupts the display.

## Modules and visibility

`crates/tui` is `pub(crate)` by default: ~100 `pub(crate) fn` against ~58 `pub
fn`, and the `pub` ones are the crate's real API surface plus what the test
harness needs. New helpers start `pub(crate)`; widen only when something
outside the crate genuinely calls it.

`crates/tui/src/lib.rs` is a declarations file. Keep it that way — new code
goes in the module it belongs to, and a new module gets a `mod` line here and
nothing else.

## Dependencies

Every dependency is declared once in the workspace `Cargo.toml` under
`[workspace.dependencies]`; member crates use `foo.workspace = true`. Don't
pin a version in a member crate.

Adding a dependency is a real decision — the binary is ~9.6 MiB and each
tree-sitter grammar is a measurable slice of that. Say what it buys in the PR.
Note the MSRV: `rust-version = "1.92"`.

## Comments and docs

Module-level `//!` headers explain what the module is for and any non-obvious
model (see `testing.rs`, `syntax/src/lib.rs`). Doc comments explain **why** and
what the constraints are, not what the next line does.

The tree has a strong existing habit of documenting *the reason a thing is the
way it is* — the locals carve-out in `syntax/src/lib.rs:195-200`, the hermetic
note in `testing.rs:48-50`. Match that: a comment that would be obvious from
the code is noise; a comment recording a constraint someone would otherwise
undo is the point.

Keep `README.md` in sync with user-visible behavior. Keep `CONTEXT.md` in sync
when a term or an architectural constraint changes — not when a feature ships.
