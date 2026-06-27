#!/bin/sh
# koma installer — https://koma.run
# Usage:
#   curl -fsSL https://koma.run/install.sh | sh
#   curl -fsSL https://koma.run/install.sh | sh -s -- --with-research
#
# Environment overrides:
#   KOMA_RELEASE_BASE   override the base download URL
#   KOMA_INSTALL_DIR    override the install directory (default /usr/local/bin)

set -e

# Base download URL (GitHub latest release). Override with KOMA_RELEASE_BASE=...
KOMA_RELEASE_BASE="${KOMA_RELEASE_BASE:-https://github.com/aula-id/koma/releases/latest/download}"

INSTALL_DIR="${KOMA_INSTALL_DIR:-/usr/local/bin}"

WITH_RESEARCH=0
for arg in "$@"; do
    case "$arg" in
        --with-research) WITH_RESEARCH=1 ;;
    esac
done

# ---------------------------------------------------------------------------
# OS detection
# ---------------------------------------------------------------------------
_os=$(uname -s)
case "$_os" in
    Linux)  os="linux"  ;;
    Darwin) os="darwin" ;;
    *)
        echo "ERROR: unsupported operating system: $_os" >&2
        echo "koma currently supports Linux and macOS." >&2
        exit 1
        ;;
esac

# ---------------------------------------------------------------------------
# Architecture detection
# ---------------------------------------------------------------------------
_arch=$(uname -m)
case "$_arch" in
    x86_64|amd64)   arch="x86_64" ;;
    aarch64|arm64)  arch="arm64"  ;;
    *)
        echo "ERROR: unsupported architecture: $_arch" >&2
        echo "koma currently supports x86_64 and arm64." >&2
        exit 1
        ;;
esac

# ---------------------------------------------------------------------------
# Supported platform gate
# ---------------------------------------------------------------------------
# Currently only Linux x86_64 is supported.
if [ "$os" != "linux" ] || [ "$arch" != "x86_64" ]; then
    echo "ERROR: koma currently supports Linux x86_64 only." >&2
    echo "Detected ${os}/${arch}, which is not supported yet." >&2
    echo "Linux arm64 and macOS builds are coming soon." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Build asset URL
# ---------------------------------------------------------------------------
# The current release publishes a single Linux x86_64 binary asset named
# "koma-linux-64". It is installed as "koma" at $INSTALL_DIR/koma.
# TODO(koma.run): when per-platform artifacts exist, publish koma-linux-arm64 and macOS builds.
asset="koma-linux-64"
url="${KOMA_RELEASE_BASE}/${asset}"

echo "koma installer — detected ${os}/${arch}"
echo "  url:      $url"
echo "  install:  $INSTALL_DIR/koma"
echo ""

# ---------------------------------------------------------------------------
# Temp file + cleanup trap
# ---------------------------------------------------------------------------
tmp=$(mktemp)
cleanup() {
    rm -f "$tmp"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Download
# ---------------------------------------------------------------------------
echo "Downloading koma..."
if command -v curl > /dev/null 2>&1; then
    curl -fsSL "$url" -o "$tmp" || {
        echo "ERROR: download failed from $url" >&2
        exit 1
    }
elif command -v wget > /dev/null 2>&1; then
    wget -qO "$tmp" "$url" || {
        echo "ERROR: download failed from $url" >&2
        exit 1
    }
else
    echo "ERROR: neither curl nor wget found; please install one and retry." >&2
    exit 1
fi

chmod +x "$tmp"

# ---------------------------------------------------------------------------
# Install — fall back to sudo if the directory is not user-writable
# ---------------------------------------------------------------------------
if [ -w "$INSTALL_DIR" ]; then
    mv "$tmp" "$INSTALL_DIR/koma"
else
    if [ "$(id -u)" = "0" ]; then
        mv "$tmp" "$INSTALL_DIR/koma"
    else
        echo "  $INSTALL_DIR is not writable; using sudo for install step."
        sudo mv "$tmp" "$INSTALL_DIR/koma"
        sudo chmod +x "$INSTALL_DIR/koma"
    fi
fi

# ---------------------------------------------------------------------------
# Optional: provision Python research environment
# ---------------------------------------------------------------------------
if [ "$WITH_RESEARCH" = "1" ]; then
    echo ""
    echo "Provisioning full internet mode environment (downloads ~80MB Firefox)..."
    "$INSTALL_DIR/koma" --internet-fullmode-install
fi

# ---------------------------------------------------------------------------
# Success
# ---------------------------------------------------------------------------
echo ""
echo "koma installed to $INSTALL_DIR/koma"
echo ""
echo "  Run 'koma' to start."
echo "  Re-run this installer with --with-research (or run"
echo "  'koma --internet-fullmode-install') to enable full internet mode."
echo ""

# Warn if install dir may not be on PATH
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        echo "  NOTE: $INSTALL_DIR does not appear to be in your PATH."
        echo "  Add the following to your shell profile and restart your terminal:"
        echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
        echo ""
        ;;
esac

echo "  https://koma.run"
