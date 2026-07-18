#!/usr/bin/env bash
set -euo pipefail

# cargo-installed tools (cargo-zigbuild) live here; login shells have it on
# PATH but non-interactive invocations may not.
export PATH="$HOME/.cargo/bin:$PATH"

DRY_RUN=false
ASSETS_ONLY=false
TAG=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      DRY_RUN=true
      shift
      ;;
    --assets-only)
      ASSETS_ONLY=true
      TAG="${2:-}"
      if [[ -z "$TAG" ]]; then
        echo "ERROR: --assets-only requires a tag argument, e.g. --assets-only v0.8.0" >&2
        exit 1
      fi
      shift 2
      ;;
    *)
      echo "ERROR: unknown argument '$1'" >&2
      exit 1
      ;;
  esac
done

if $DRY_RUN; then
  echo "==> Dry-run mode: builds will run but no tag/push/release will happen"
fi

# ---------------------------------------------------------------------------
# 1. Determine version/tag
# ---------------------------------------------------------------------------
if $ASSETS_ONLY; then
  echo "==> Assets-only mode: rebuilding archives for existing tag ${TAG}"
else
  VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
  TAG="v${VERSION}"
  echo "==> Version: ${VERSION}  Tag: ${TAG}"
fi

# ---------------------------------------------------------------------------
# 2. Guard
# ---------------------------------------------------------------------------
if $ASSETS_ONLY; then
  # Re-publishing assets only applies to a release that already exists.
  if ! git rev-parse "$TAG" &>/dev/null; then
    echo "ERROR: tag ${TAG} does not exist — --assets-only requires an existing tag" >&2
    exit 1
  fi

  if ! gh release view "$TAG" &>/dev/null; then
    echo "ERROR: no GitHub release found for tag ${TAG}" >&2
    exit 1
  fi
else
  # Guard: must be on main with a clean tree
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
fi

# ---------------------------------------------------------------------------
# 3. Check prerequisites
#    zig + cargo-zigbuild handle the linux cross-builds (the tree-sitter C
#    grammars compile fine under zig cc — v0.7.0/v0.8.0 shipped this way).
# ---------------------------------------------------------------------------
for cmd in cargo zig gh; do
  if ! command -v "$cmd" &>/dev/null; then
    echo "ERROR: '${cmd}' not found on PATH" >&2
    echo "  Run: brew install zig  (for zig)" >&2
    echo "       cargo install cargo-zigbuild  (for zigbuild)" >&2
    echo "       brew install gh  (for GitHub CLI)" >&2
    exit 1
  fi
done

if ! command -v cargo-zigbuild &>/dev/null; then
  echo "ERROR: cargo-zigbuild not installed — run: cargo install cargo-zigbuild" >&2
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

echo "==> Building x86_64-unknown-linux-gnu"
cargo zigbuild --release --target x86_64-unknown-linux-gnu

echo "==> Building aarch64-unknown-linux-gnu"
cargo zigbuild --release --target aarch64-unknown-linux-gnu

# ---------------------------------------------------------------------------
# 5. Package .tar.gz archives
#    Names MUST stay vix-<TAG>-<os>-<arch>.tar.gz — install.sh reconstructs
#    them to build its download URL.
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
package "aarch64-apple-darwin"       "vix-${TAG}-darwin-arm64.tar.gz"
package "x86_64-unknown-linux-gnu"   "vix-${TAG}-linux-x86_64.tar.gz"
package "aarch64-unknown-linux-gnu"  "vix-${TAG}-linux-arm64.tar.gz"

if $DRY_RUN; then
  echo "==> Dry-run complete. Archives in ${DIST}/:"
  ls -lh "$DIST/"
  exit 0
fi

if $ASSETS_ONLY; then
  # -------------------------------------------------------------------------
  # 6. Upload archives to the existing release (no tag/push/create)
  # -------------------------------------------------------------------------
  echo "==> Uploading assets to existing release ${TAG}"
  gh release upload "$TAG" "${DIST}"/*.tar.gz --clobber

  echo "==> Done! https://github.com/$(gh repo view --json nameWithOwner -q .nameWithOwner)/releases/tag/${TAG}"
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
