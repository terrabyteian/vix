# Release process

`scripts/release.sh` is the whole process. It is shared verbatim with the
sibling repo (rug / vix) apart from a two-line config block at the top; keep
them identical and copy fixes across.

## Prerequisites (one-time setup)

```bash
brew install zig gh            # zig cc drives the Linux cross-builds; gh talks to GitHub
cargo install cargo-zigbuild
rustup target add aarch64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
gh auth login                  # if not already authenticated
```

## Cutting a release

```bash
scripts/release.sh 0.9.0
```

From a clean checkout of the default branch that is not behind origin, the
script:

1. Refuses to run if the tag or GitHub release already exists.
2. Bumps `version` in `Cargo.toml` and refreshes `Cargo.lock`.
3. Runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`
   (`--skip-checks` bypasses them).
4. Builds darwin-arm64 natively and both Linux targets with `cargo zigbuild`,
   packaging each as `dist/<name>-v0.9.0-<os>-<arch>.tar.gz`.
5. Only then commits the bump (`chore: bump version to 0.9.0`), creates the
   annotated tag `v0.9.0`, pushes branch + tag, and creates the GitHub
   release with `--generate-notes` and the three archives attached.
6. Verifies the release has all three assets.

If anything fails before step 5 the version bump is reverted and nothing has
been pushed. Release notes come from commit messages, so write them for a
reader of the changelog.

## Dry run

```bash
scripts/release.sh --dry-run          # checks + builds at the current version
scripts/release.sh --dry-run 0.9.0    # same, exercising the bump; reverted on exit
```

## Re-publishing assets for an existing tag

If a release exists but its archives are wrong or missing:

```bash
scripts/release.sh --assets-only v0.9.0
```

Rebuilds the three archives and uploads them with `--clobber`. Does not touch
git. Combine with `--dry-run` to only rebuild into `dist/`.

## Artifact naming

Archive names are load-bearing: `install.sh` reconstructs
`<name>-<tag>-<os>-<arch>.tar.gz` to build its download URL. Never rename
release assets by hand.

| File | Target |
|------|--------|
| `<name>-vX.Y.Z-darwin-arm64.tar.gz` | `aarch64-apple-darwin` |
| `<name>-vX.Y.Z-linux-x86_64.tar.gz` | `x86_64-unknown-linux-gnu` |
| `<name>-vX.Y.Z-linux-arm64.tar.gz` | `aarch64-unknown-linux-gnu` |

Each archive contains the single binary. No native darwin-x86_64 build is
shipped; Intel Macs use Rosetta 2.
