#!/usr/bin/env sh
# Installer. Downloads the release archive for this machine's OS/arch from
# GitHub Releases and places the binary in ~/.local/bin.
#
# Default (latest release):
#   curl -fsSL https://raw.githubusercontent.com/terrabyteian/vix/main/install.sh | sh
#
# Specific version:
#   curl -fsSL https://raw.githubusercontent.com/terrabyteian/vix/main/install.sh | VIX_VERSION=v0.9.0 sh
#
# Override the install location with VIX_INSTALL_DIR.
#
# This script is shared verbatim between the rug and vix repos apart from
# the config block below. Keep it that way: fix a bug here, copy it there.
set -eu

# --- Project config (the only lines that differ between repos) --------------
REPO="terrabyteian/vix"
BINARY="vix"
ENV_PREFIX="VIX"   # honours ${ENV_PREFIX}_VERSION and ${ENV_PREFIX}_INSTALL_DIR
# ---------------------------------------------------------------------------

eval "VERSION=\"\${${ENV_PREFIX}_VERSION:-}\""
eval "INSTALL_DIR=\"\${${ENV_PREFIX}_INSTALL_DIR:-\$HOME/.local/bin}\""

# ---------------------------------------------------------------------------
# Detect OS / architecture
# ---------------------------------------------------------------------------
OS="$(uname -s)"
case "$OS" in
  Darwin) OS="darwin" ;;
  Linux)  OS="linux"  ;;
  *)
    echo "error: unsupported OS: $OS" >&2
    exit 1
    ;;
esac

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64)           ARCH="x86_64" ;;
  aarch64 | arm64)  ARCH="arm64"  ;;
  *)
    echo "error: unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

# Only darwin-arm64 is shipped for macOS; x86_64 Macs can run it via Rosetta
# but we don't ship a native darwin-x86_64 binary.
if [ "$OS" = "darwin" ] && [ "$ARCH" = "x86_64" ]; then
  echo "error: no native darwin-x86_64 build is available." >&2
  echo "       Intel Macs can run the arm64 build via Rosetta 2." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Resolve version (env override or latest from GitHub API)
# ---------------------------------------------------------------------------
if [ -z "$VERSION" ]; then
  printf "==> Fetching latest release... "
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' \
    | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
  echo "$VERSION"
fi
[ -n "$VERSION" ] || { echo "error: could not determine latest release" >&2; exit 1; }

# Normalise: ensure leading 'v'.
case "$VERSION" in
  v*) ;;
  *)  VERSION="v${VERSION}" ;;
esac

echo "==> Installing ${BINARY} ${VERSION} (${OS}-${ARCH})"

# ---------------------------------------------------------------------------
# Download
# ---------------------------------------------------------------------------
ARCHIVE="${BINARY}-${VERSION}-${OS}-${ARCH}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "==> Downloading ${URL}"
curl -fsSL "$URL" -o "${TMP}/${ARCHIVE}"
tar -xzf "${TMP}/${ARCHIVE}" -C "$TMP"

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------
mkdir -p "$INSTALL_DIR" 2>/dev/null || sudo mkdir -p "$INSTALL_DIR"

if [ -w "$INSTALL_DIR" ]; then
  SUDO=""
else
  echo "==> ${INSTALL_DIR} is not writable — using sudo"
  SUDO="sudo"
fi

# Stage inside INSTALL_DIR so the final step is a same-filesystem rename().
# Copying over the existing binary would reuse its inode, and macOS caches a
# binary's code signature per inode: the kernel would check the new bytes
# against the old cached hash and SIGKILL it at exec as "Code Signature
# Invalid". A rename gives the new binary its own inode.
STAGE="${INSTALL_DIR}/.${BINARY}.new.$$"
trap 'rm -rf "$TMP"; $SUDO rm -f "$STAGE"' EXIT

$SUDO cp "${TMP}/${BINARY}" "$STAGE"
$SUDO chmod 755 "$STAGE"
$SUDO mv -f "$STAGE" "${INSTALL_DIR}/${BINARY}"

echo "==> Installed: ${INSTALL_DIR}/${BINARY} ($("${INSTALL_DIR}/${BINARY}" --version))"

# Warn if the install dir isn't on PATH (common for ~/.local/bin on a fresh setup).
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo "==> note: ${INSTALL_DIR} is not on your PATH — add it with:"
    echo "         export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
