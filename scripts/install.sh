#!/bin/sh
# zap-mcp installer for macOS.
#
#   curl -fsSL https://github.com/don41382/zap-mcp/releases/latest/download/install.sh | sh
#
# Downloads the release archive for this Mac, verifies it against the
# release's SHA256SUMS.txt and starts the guided setup, which installs
# zap-mcp to ~/.local/bin (or offers update / uninstall if it's installed).
# Set ZAP_MCP_VERSION=v0.3.0 to pick a specific release.
set -eu

REPO="don41382/zap-mcp"
VERSION="${ZAP_MCP_VERSION:-latest}"
if [ -n "${ZAP_MCP_DOWNLOAD_URL:-}" ]; then
    BASE="$ZAP_MCP_DOWNLOAD_URL"
elif [ "$VERSION" = "latest" ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
else
    BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

case "$(uname -s)" in
    Darwin) ;;
    *) echo "This installer is for macOS. On Windows, use install.ps1." >&2; exit 1 ;;
esac
case "$(uname -m)" in
    arm64) TARGET="aarch64-apple-darwin" ;;
    x86_64) TARGET="x86_64-apple-darwin" ;;
    *) echo "Unsupported CPU: $(uname -m)" >&2; exit 1 ;;
esac

ARCHIVE="zap-mcp-$TARGET.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

echo "Downloading $ARCHIVE ..."
curl -fsSL "$BASE/$ARCHIVE" -o "$TMP/$ARCHIVE"
curl -fsSL "$BASE/SHA256SUMS.txt" -o "$TMP/SHA256SUMS.txt"

if ! (cd "$TMP" && grep " $ARCHIVE\$" SHA256SUMS.txt | shasum -a 256 -c - >/dev/null 2>&1); then
    echo "Checksum verification failed; nothing was installed." >&2
    exit 1
fi
echo "Checksum OK."

tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
# Setup is interactive; with `curl | sh` our stdin is the pipe, so read
# answers from the terminal instead.
"$TMP/zap-mcp-$TARGET/zap-mcp" setup --from-installer </dev/tty
