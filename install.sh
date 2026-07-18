#!/usr/bin/env sh
set -eu

REPO="ta17eee/claude-code-statusline"
DEST="$HOME/.claude/statusline"

os=$(uname -s)
arch=$(uname -m)

case "$os" in
  Darwin)
    case "$arch" in
      arm64)  asset="statusline-macos-arm64" ;;
      x86_64) asset="statusline-macos-x86_64" ;;
      *) echo "Unsupported macOS architecture: $arch" >&2; exit 1 ;;
    esac
    ;;
  Linux)
    case "$arch" in
      x86_64) asset="statusline-linux-x86_64" ;;
      *) echo "Unsupported Linux architecture: $arch" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $os (Windows users: see install.ps1 or download a binary from the Releases page)" >&2
    exit 1
    ;;
esac

url="https://github.com/$REPO/releases/latest/download/$asset"

mkdir -p "$HOME/.claude"
echo "Downloading $asset..."
curl -fLo "$DEST" "$url"
chmod +x "$DEST"

echo "Installed to $DEST"
echo ""
echo "Add this to ~/.claude/settings.json:"
echo '  "statusLine": { "type": "command", "command": "'"$DEST"'" }'
echo '  "subagentStatusLine": { "type": "command", "command": "'"$DEST"' --subagent" }'
