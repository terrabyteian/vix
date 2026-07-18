# Agent notes for vix

vix is a modal (vim-flavored) terminal editor. Cargo workspace: `crates/core`
(editing engine), `crates/tui` (ratatui frontend), `crates/syntax`
(tree-sitter highlighting), `crates/picker`, `crates/lsp`, `crates/app`
(the `vix` binary).

- `PROGRESS.md` is the single source of truth for implementation state —
  append a section at the end of each work pass (see existing sections for
  the format: what shipped, file-level notes, verification).
- Keep `README.md` in sync with user-visible behavior changes.
- Before any release: `cargo test --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`. Known environmental failure: the `vix-lsp`
  smoke test needs a spawnable rust-analyzer.

## Release process

Everything is driven by `scripts/release.sh`. Do not release by hand — the
archive names are load-bearing: `install.sh` reconstructs
`vix-<tag>-<os>-<arch>.tar.gz` (`darwin-arm64`, `linux-x86_64`,
`linux-arm64`) to build its download URL, so a rename breaks
`curl | sh` installs.

1. **Bump the version** in the workspace `Cargo.toml` (single `version`
   field; crates inherit it), then `cargo update --workspace` so
   `Cargo.lock` follows. Add the release's `PROGRESS.md` section. Commit
   (e.g. `Bump to 0.8.0; PROGRESS notes for <pass>`), push `main`.
2. **Run `scripts/release.sh`**. It parses the version from `Cargo.toml`,
   refuses to run off `main` or with a dirty tree, builds
   aarch64-apple-darwin natively plus both Linux targets via
   `cargo zigbuild`, packages `dist/*.tar.gz`, tags `v<version>`, pushes the
   tag, and creates the GitHub release with `--generate-notes` and the three
   archives attached.
   - `--dry-run`: build + package only; no tag/push/release.
   - `--assets-only v<X.Y.Z>`: rebuild and re-upload (`--clobber`) archives
     to an existing release — the recovery path for wrong/missing assets.

### Toolchain (host: darwin-arm64)

- `cargo-zigbuild` + `zig` do the Linux cross-builds; the tree-sitter C
  grammars compile fine under zig cc (v0.7.0 and v0.8.0 shipped this way).
  Do NOT reintroduce `cross`/Docker — this machine has neither.
- Requires rustup targets `x86_64-unknown-linux-gnu`,
  `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`.
- `cargo-zigbuild` lives in `~/.cargo/bin`, which non-interactive shells may
  not have on PATH; the script exports it itself.
- Git identity is repo-local on this machine (global is unset); commits in
  fresh clones will need `git config user.name/user.email` first.
