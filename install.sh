#!/usr/bin/env sh
# vix installer
#
# Default (latest release):
#   curl -fsSL https://raw.githubusercontent.com/terrabyteian/vix/main/install.sh | sh
#
# Specific version:
#   curl -fsSL https://raw.githubusercontent.com/terrabyteian/vix/main/install.sh | VIX_VERSION=v0.1.0 sh
set -e

REPO="terrabyteian/vix"
BINARY="vix"
INSTALL_DIR="${VIX_INSTALL_DIR:-/usr/local/bin}"

# ---------------------------------------------------------------------------
# Detect OS
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

# ---------------------------------------------------------------------------
# Detect architecture
# ---------------------------------------------------------------------------
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
if [ -z "${VIX_VERSION:-}" ]; then
  printf "==> Fetching latest release... "
  VIX_VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' \
    | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
  echo "$VIX_VERSION"
fi

# Normalise: ensure leading 'v'.
case "$VIX_VERSION" in
  v*) ;;
  *)  VIX_VERSION="v${VIX_VERSION}" ;;
esac

echo "==> Installing ${BINARY} ${VIX_VERSION} (${OS}-${ARCH})"

# ---------------------------------------------------------------------------
# Download
# ---------------------------------------------------------------------------
ARCHIVE="${BINARY}-${VIX_VERSION}-${OS}-${ARCH}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VIX_VERSION}/${ARCHIVE}"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "==> Downloading ${URL}"
curl -fsSL "$URL" -o "${TMP}/${ARCHIVE}"
tar -xzf "${TMP}/${ARCHIVE}" -C "$TMP"

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------
if [ ! -d "$INSTALL_DIR" ]; then
  mkdir -p "$INSTALL_DIR"
fi

if [ -w "$INSTALL_DIR" ]; then
  cp "${TMP}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
  chmod +x "${INSTALL_DIR}/${BINARY}"
else
  echo "==> ${INSTALL_DIR} is not writable — using sudo"
  sudo cp "${TMP}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
  sudo chmod +x "${INSTALL_DIR}/${BINARY}"
fi

echo "==> Installed: $(command -v ${BINARY} || echo ${INSTALL_DIR}/${BINARY})"
"${INSTALL_DIR}/${BINARY}" --version
