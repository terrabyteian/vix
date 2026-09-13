# Agent notes for vix

vix is a modal (vim-flavored) terminal editor. Cargo workspace: `crates/core`
(editing engine), `crates/tui` (ratatui frontend), `crates/syntax`
(tree-sitter highlighting), `crates/picker`, `crates/lsp`, `crates/app`
(the `vix` binary).

- `CONTEXT.md` (repo root) holds the project's vocabulary, its architectural
  commitments, and its deliberate non-goals. Read it before proposing a
  design; update it when a term or constraint actually changes. It is not a
  work log — what shipped when lives in git history.
- `docs/agents/standards.md` documents this repo's coding standards — test
  placement, the `Harness` contract, error layering, visibility defaults,
  dependency policy. `/code-review` reviews against it.
- Keep `README.md` in sync with user-visible behavior changes.
- `scripts/release.sh` runs `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and
  `cargo test --workspace` before building; run them yourself before pushing
  too. The `vix-lsp` smoke test needs a working rust-analyzer and skips
  itself (still green) when there isn't one — e.g. when only the rustup shim
  is present without the component installed.

## Release process

`scripts/release.sh X.Y.Z` is the whole process; full mechanics are in
`scripts/README.md`. Never release by hand. From a clean, up-to-date
`main` checkout it bumps `Cargo.toml`/`Cargo.lock`, runs
fmt/clippy/test, builds darwin-arm64 natively and both Linux targets via
`cargo zigbuild`, packages `dist/*.tar.gz`, and only then commits the bump
(`chore: bump version to X.Y.Z`), creates the annotated tag `vX.Y.Z`, pushes
branch + tag, and creates the GitHub release with `--generate-notes` and the
three archives attached. Release notes are generated from commit messages,
so write them for a changelog reader.

- `--dry-run [X.Y.Z]`: checks + build + package only; a bump is reverted on
  exit.
- `--assets-only vX.Y.Z`: rebuild and re-upload archives to an existing
  release (recovery path for wrong/missing assets).
- `--skip-checks`: bypass fmt/clippy/test.

Archive names are load-bearing: `install.sh` reconstructs
`vix-<tag>-<os>-<arch>.tar.gz` (`darwin-arm64`, `linux-x86_64`,
`linux-arm64`) to build its download URL. `scripts/release.sh` and
`install.sh` are shared verbatim with the `rug` repo apart from the
config block at the top of each; copy fixes across rather than letting them
drift.

### Toolchain (host: darwin-arm64)

- `zig` (homebrew) + `cargo-zigbuild` do the Linux cross-builds; requires
  rustup targets `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`,
  `aarch64-unknown-linux-gnu`. No cross/Docker on this machine — do not
  reintroduce them.
- The tree-sitter C grammars compile fine under zig cc (every release
  since v0.7.0 shipped this way).
- `cargo-zigbuild` lives in `~/.cargo/bin`, which non-interactive shells may
  not have on PATH; the script exports it itself.
- Git identity is repo-local on this machine (global is unset); a fresh
  clone needs `git config user.name "Ian Hall"` / `git config user.email
  terrabytian@gmail.com` before committing.

## Agent skills

### Issue tracker

Issues live as GitHub issues in `terrabyteian/vix`, driven by the `gh` CLI.
See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles, used verbatim as label strings. See
`docs/agents/triage-labels.md`.

### Domain docs

Single-context: a root `CONTEXT.md` plus `docs/adr/`. See
`docs/agents/domain.md`.
