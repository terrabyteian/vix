#!/usr/bin/env bash
# Release driver. Full docs: scripts/README.md
#
#   scripts/release.sh X.Y.Z                  bump, check, build, commit, tag, push, GitHub release
#   scripts/release.sh --dry-run [X.Y.Z]      checks + builds + archives only; no git/gh writes
#   scripts/release.sh --assets-only vX.Y.Z   rebuild + re-upload archives to an existing release
#   --skip-checks                             skip fmt/clippy/test (any mode)
#
# This script is shared verbatim between the rug and vix repos apart from
# the config block below. Keep it that way: fix a bug here, copy it there.
set -euo pipefail

# --- Project config (the only lines that differ between repos) --------------
BINARY="vix"
DEFAULT_BRANCH="main"
# ---------------------------------------------------------------------------

# cargo-installed tools (cargo-zigbuild) live here; login shells have it on
# PATH but non-interactive invocations may not.
export PATH="$HOME/.cargo/bin:$PATH"
cd "$(git rev-parse --show-toplevel)"

DIST="dist"
TARGETS=(aarch64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu)

# Archive names are load-bearing: install.sh reconstructs
# <BINARY>-<tag>-<os>-<arch>.tar.gz to build its download URL.
archive_suffix() {
  case "$1" in
    aarch64-apple-darwin)      echo "darwin-arm64" ;;
    x86_64-unknown-linux-gnu)  echo "linux-x86_64" ;;
    aarch64-unknown-linux-gnu) echo "linux-arm64" ;;
    *) echo "ERROR: unknown target $1" >&2; exit 1 ;;
  esac
}

die()  { echo "ERROR: $*" >&2; exit 1; }
step() { echo "==> $*"; }

# ---------------------------------------------------------------------------
# 1. Arguments
# ---------------------------------------------------------------------------
DRY_RUN=false
ASSETS_ONLY=false
SKIP_CHECKS=false
NEW_VERSION=""
TAG=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)     DRY_RUN=true; shift ;;
    --skip-checks) SKIP_CHECKS=true; shift ;;
    --assets-only)
      ASSETS_ONLY=true
      TAG="${2:-}"
      [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "--assets-only needs a tag like v0.8.0"
      shift 2 ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    -*) die "unknown flag '$1'" ;;
    *)
      [[ -z "$NEW_VERSION" ]] || die "unexpected argument '$1'"
      [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must look like X.Y.Z (got '$1')"
      NEW_VERSION="$1"; shift ;;
  esac
done

current_version() { grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/'; }

if $ASSETS_ONLY; then
  [[ -z "$NEW_VERSION" ]] || die "--assets-only takes a tag, not a new version"
  step "Assets-only mode: rebuilding archives for existing release ${TAG}"
elif [[ -n "$NEW_VERSION" ]]; then
  TAG="v${NEW_VERSION}"
  step "Releasing ${BINARY} $(current_version) -> ${NEW_VERSION} (tag ${TAG})"
else
  $DRY_RUN || die "a new version is required, e.g. scripts/release.sh 0.9.0 (or --dry-run to build the current version)"
  TAG="v$(current_version)"
  step "Dry run at current version $(current_version)"
fi
$DRY_RUN && step "Dry-run mode: no commit/tag/push/release will happen"

# ---------------------------------------------------------------------------
# 2. Guards
# ---------------------------------------------------------------------------
for cmd in cargo cargo-zigbuild zig gh git; do
  command -v "$cmd" >/dev/null 2>&1 || die "'${cmd}' not found on PATH — see scripts/README.md prerequisites"
done

if $ASSETS_ONLY; then
  git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null || die "tag ${TAG} does not exist locally"
  gh release view "$TAG" >/dev/null 2>&1 || die "no GitHub release found for ${TAG}"
  # The archives replace that release's assets, so they must be built from
  # the code the tag names — not from whatever happens to be checked out.
  [[ "$(git rev-parse HEAD)" == "$(git rev-parse "${TAG}^{commit}")" ]] \
    || die "HEAD is not ${TAG} — run 'git checkout ${TAG}' first so the rebuilt archives match the release"
  [[ -z "$(git status --porcelain)" ]] || die "working tree is not clean — the rebuilt archives would not match ${TAG}"
else
  BRANCH="$(git rev-parse --abbrev-ref HEAD)"
  [[ "$BRANCH" == "$DEFAULT_BRANCH" ]] || die "must be on ${DEFAULT_BRANCH} (currently on '${BRANCH}')"

  if [[ -n "$(git status --porcelain)" ]]; then
    git status --short >&2
    die "working tree is not clean — commit, stash, or remove the above first"
  fi

  if ! $DRY_RUN; then
    git fetch -q origin "$DEFAULT_BRANCH"
    git merge-base --is-ancestor "origin/${DEFAULT_BRANCH}" HEAD \
      || die "local ${DEFAULT_BRANCH} is behind origin/${DEFAULT_BRANCH} — pull first"
    git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null && die "tag ${TAG} already exists"
    git ls-remote --exit-code --tags origin "refs/tags/${TAG}" >/dev/null 2>&1 && die "tag ${TAG} already exists on origin"
    gh release view "$TAG" >/dev/null 2>&1 && die "GitHub release ${TAG} already exists"
    gh auth status >/dev/null 2>&1 || die "gh is not authenticated — run: gh auth login"
  fi
fi

# ---------------------------------------------------------------------------
# 3. Version bump (kept uncommitted until the build succeeds)
# ---------------------------------------------------------------------------
BUMPED=false
revert_bump() {
  if $BUMPED; then
    echo "==> Reverting uncommitted version bump"
    # From HEAD, not the index: if `git commit` fails after `git add`, the
    # bump is staged and a plain checkout would restore the staged copy.
    git checkout -q HEAD -- Cargo.toml Cargo.lock
  fi
}
trap revert_bump EXIT

if [[ -n "$NEW_VERSION" ]]; then
  step "Bumping Cargo.toml to ${NEW_VERSION}"
  BUMPED=true
  perl -pi -e 'if (!$done && s/^version = "[^"]*"/version = "'"$NEW_VERSION"'"/) { $done = 1 }' Cargo.toml
  [[ "$(current_version)" == "$NEW_VERSION" ]] || die "failed to bump version in Cargo.toml"
  cargo update --workspace -q
  CHANGED="$(git diff --name-only | sort | tr '\n' ' ')"
  [[ "$CHANGED" == "Cargo.lock Cargo.toml " || "$CHANGED" == "Cargo.toml " ]] \
    || die "bump touched unexpected files: ${CHANGED}"
fi

# ---------------------------------------------------------------------------
# 4. Checks
# ---------------------------------------------------------------------------
if $SKIP_CHECKS; then
  step "Skipping fmt/clippy/test (--skip-checks)"
else
  step "cargo fmt --check";  cargo fmt --all -- --check
  step "cargo clippy";       cargo clippy --workspace --all-targets -- -D warnings
  step "cargo test";         cargo test --workspace
fi

# ---------------------------------------------------------------------------
# 5. Build + package
# ---------------------------------------------------------------------------
rm -rf "$DIST"
mkdir -p "$DIST"

for target in "${TARGETS[@]}"; do
  if [[ "$target" == *apple-darwin ]]; then
    step "Building ${target} (native)"
    cargo build --release --target "$target"
  else
    step "Building ${target} (cargo zigbuild)"
    cargo zigbuild --release --target "$target"
  fi

  binary="target/${target}/release/${BINARY}"
  [[ -f "$binary" ]] || die "binary not found at ${binary}"
  archive="${BINARY}-${TAG}-$(archive_suffix "$target").tar.gz"
  tar -czf "${DIST}/${archive}" -C "$(dirname "$binary")" "$(basename "$binary")"
  echo "    packaged ${DIST}/${archive}"
done

if $DRY_RUN; then
  step "Dry run complete. Archives in ${DIST}/:"
  ls -lh "$DIST/"
  exit 0
fi

# ---------------------------------------------------------------------------
# 6. Publish
# ---------------------------------------------------------------------------
if $ASSETS_ONLY; then
  step "Uploading archives to existing release ${TAG}"
  gh release upload "$TAG" "${DIST}"/*.tar.gz --clobber
else
  step "Committing version bump"
  git add Cargo.toml Cargo.lock
  git commit -q -m "chore: bump version to ${NEW_VERSION}"
  BUMPED=false   # committed; nothing to revert from here on

  step "Tagging ${TAG}"
  git tag -a "$TAG" -m "${BINARY} ${TAG}"

  step "Pushing ${DEFAULT_BRANCH} and ${TAG}"
  git push origin "$DEFAULT_BRANCH" "$TAG"

  step "Creating GitHub release ${TAG}"
  gh release create "$TAG" \
    --title "${BINARY} ${TAG}" \
    --generate-notes \
    "${DIST}"/*.tar.gz
fi

# ---------------------------------------------------------------------------
# 7. Verify
# ---------------------------------------------------------------------------
n="$(gh release view "$TAG" --json assets -q '.assets | length')"
[[ "$n" == "${#TARGETS[@]}" ]] || die "release ${TAG} has ${n} assets, expected ${#TARGETS[@]}"
step "Done: https://github.com/$(gh repo view --json nameWithOwner -q .nameWithOwner)/releases/tag/${TAG}"
