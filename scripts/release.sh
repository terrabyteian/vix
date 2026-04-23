#!/usr/bin/env bash
set -euo pipefail

DRY_RUN=false
if [[ "${1:-}" == "--dry-run" ]]; then
  DRY_RUN=true
  echo "==> Dry-run mode: builds will run but no tag/push/release will happen"
fi

# ---------------------------------------------------------------------------
# 1. Parse version from the workspace Cargo.toml
# ---------------------------------------------------------------------------
VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
TAG="v${VERSION}"
echo "==> Version: ${VERSION}  Tag: ${TAG}"

# ---------------------------------------------------------------------------
# 2. Guard: must be on main with a clean tree
# ---------------------------------------------------------------------------
BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [[ "$BRANCH" != "main" ]]; then
  echo "ERROR: must be on main branch (currently on '${BRANCH}')" >&2
  exit 1
fi

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "ERROR: working tree is not clean — commit or stash changes first" >&2
  exit 1
fi

if ! $DRY_RUN && git rev-parse "$TAG" &>/dev/null; then
  echo "ERROR: tag ${TAG} already exists" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 3. Check prerequisites
#    - `cargo`: native darwin build
#    - `cross` + `docker`: linux cross-builds (tree-sitter grammars make
#      cargo-zigbuild fragile; cross's docker toolchain is reliable)
#    - `gh`: GitHub CLI for the release upload
# ---------------------------------------------------------------------------
for cmd in cargo cross gh docker; do
  if ! command -v "$cmd" &>/dev/null; then
    echo "ERROR: '${cmd}' not found on PATH" >&2
    echo "" >&2
    echo "  cross: cargo install cross --git https://github.com/cross-rs/cross --locked" >&2
    echo "         (the released 0.2.5 doesn't work on darwin-arm64 hosts; we need main)" >&2
    echo "  docker: brew install --cask docker" >&2
    echo "  gh: brew install gh" >&2
    exit 1
  fi
done

if ! docker info &>/dev/null; then
  echo "ERROR: Docker daemon is not running — start Docker Desktop first" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 4. Build
# ---------------------------------------------------------------------------
DIST="dist"
rm -rf "$DIST"
mkdir -p "$DIST"

echo "==> Building aarch64-apple-darwin (native)"
cargo build --release --target aarch64-apple-darwin

echo "==> Building x86_64-unknown-linux-gnu (via cross)"
cross build --release --target x86_64-unknown-linux-gnu

echo "==> Building aarch64-unknown-linux-gnu (via cross)"
cross build --release --target aarch64-unknown-linux-gnu

# ---------------------------------------------------------------------------
# 5. Package .tar.gz archives
# ---------------------------------------------------------------------------
package() {
  local target="$1"
  local archive_name="$2"
  local binary="target/${target}/release/vix"

  if [[ ! -f "$binary" ]]; then
    echo "ERROR: binary not found at ${binary}" >&2
    exit 1
  fi

  tar -czf "${DIST}/${archive_name}" -C "$(dirname "$binary")" "$(basename "$binary")"
  echo "    created ${DIST}/${archive_name}"
}

echo "==> Packaging archives"
package "aarch64-apple-darwin"      "vix-${TAG}-darwin-arm64.tar.gz"
package "x86_64-unknown-linux-gnu"  "vix-${TAG}-linux-x86_64.tar.gz"
package "aarch64-unknown-linux-gnu" "vix-${TAG}-linux-arm64.tar.gz"

if $DRY_RUN; then
  echo "==> Dry-run complete. Archives in ${DIST}/:"
  ls -lh "$DIST/"
  exit 0
fi

# ---------------------------------------------------------------------------
# 6. Tag and push
# ---------------------------------------------------------------------------
echo "==> Tagging ${TAG}"
git tag "$TAG"

echo "==> Pushing tag"
git push origin "$TAG"

# ---------------------------------------------------------------------------
# 7. Create GitHub Release with auto-generated notes and upload assets
# ---------------------------------------------------------------------------
echo "==> Creating GitHub Release ${TAG}"
gh release create "$TAG" \
  --title "vix ${TAG}" \
  --generate-notes \
  "${DIST}"/*.tar.gz

echo "==> Done! https://github.com/$(gh repo view --json nameWithOwner -q .nameWithOwner)/releases/tag/${TAG}"
