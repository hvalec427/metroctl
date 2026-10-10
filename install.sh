#!/bin/sh
# Install metroctl into ~/.metroctl/bin (no sudo) and put that on your PATH.
#   curl -fsSL https://raw.githubusercontent.com/hvalec427/metroctl/master/install.sh | sh
#   … | sh -s -- dev        rolling dev build (or `nightly`, or a version tag)
set -e

TOOL="metroctl"
REPO="hvalec427/metroctl"
BIN_DIR="$HOME/.$TOOL/bin"

# Detect architecture
ARCH=$(uname -m)
if [ "$ARCH" = "arm64" ]; then
  FILE="$TOOL-darwin-arm64"
else
  FILE="$TOOL-darwin-x64"
fi

# `dev` → rolling dev build; `nightly` → newest nightly; a version string → that
# exact tag; else latest stable.
if [ "$1" = "dev" ]; then
  VERSION="dev"
elif [ "$1" = "nightly" ]; then
  # GitHub's /releases list isn't newest-first — version-sort and take the highest.
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=30" \
    | grep '"tag_name"' | grep nightly | cut -d'"' -f4 | sort -V | tail -1)
elif [ -n "$1" ]; then
  VERSION="$1"
else
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep '"tag_name"' | head -1 | cut -d'"' -f4)
fi

if [ -z "$VERSION" ]; then
  echo "Error: could not resolve a release from $REPO"
  exit 1
fi

# The channel this build belongs to; `$TOOL update` stays on it.
case "$VERSION" in
  dev) CHANNEL="dev" ;;
  *-dev.*) CHANNEL="dev" ;;
  *-nightly.*) CHANNEL="nightly" ;;
  *) CHANNEL="stable" ;;
esac

URL="https://github.com/$REPO/releases/download/$VERSION/$FILE"
TMP="$(mktemp -t "$TOOL")"

echo "Installing $TOOL $VERSION ($ARCH) to $BIN_DIR/${TOOL}…"
curl -fsSL "$URL" -o "$TMP"
chmod +x "$TMP"
# Strip the macOS quarantine flag so Gatekeeper doesn't block the (un-notarized)
# binary with "Apple could not verify ... free of malware". No-op off macOS.
xattr -d com.apple.quarantine "$TMP" 2>/dev/null || true
mkdir -p "$BIN_DIR"
# mv (a new file), never overwrite in place: macOS kills a binary whose
# signature changed under it.
mv -f "$TMP" "$BIN_DIR/$TOOL"

echo "Done — $TOOL $VERSION ($CHANNEL channel) installed to $BIN_DIR/$TOOL"

# Put BIN_DIR on PATH in the user's shell config (once).
case ":$PATH:" in
  *":$BIN_DIR:"*) ON_PATH=1 ;;
  *) ON_PATH=0 ;;
esac
case "$BIN_DIR" in
  "$HOME"/*) PATH_DIR="\$HOME${BIN_DIR#"$HOME"}" ;;
  *) PATH_DIR="$BIN_DIR" ;;
esac
SHELL_NAME=$(basename "${SHELL:-sh}")
case "$SHELL_NAME" in
  zsh) RC="${ZDOTDIR:-$HOME}/.zshrc"; LINE="export PATH=\"$PATH_DIR:\$PATH\"" ;;
  bash)
    if [ -f "$HOME/.bash_profile" ]; then RC="$HOME/.bash_profile"; else RC="$HOME/.bashrc"; fi
    LINE="export PATH=\"$PATH_DIR:\$PATH\"" ;;
  fish) RC="$HOME/.config/fish/config.fish"; LINE="fish_add_path $PATH_DIR" ;;
  *) RC="$HOME/.profile"; LINE="export PATH=\"$PATH_DIR:\$PATH\"" ;;
esac
if ! grep -qsF "$LINE" "$RC"; then
  mkdir -p "$(dirname "$RC")"
  printf '\n# %s\n%s\n' "$TOOL" "$LINE" >> "$RC"
  echo "Added $PATH_DIR to PATH in $RC."
fi
if [ "$ON_PATH" = 0 ]; then
  echo "Open a new terminal, or run:  $LINE"
fi

# Older copies elsewhere on PATH (e.g. /usr/local/bin from a previous installer).
OLD=$(which -a "$TOOL" 2>/dev/null | grep -vxF "$BIN_DIR/$TOOL" | sort -u || true)
if [ -n "$OLD" ]; then
  echo "Note: other copies of $TOOL on your PATH (the new one comes first once PATH is updated):"
  echo "$OLD" | sed 's/^/  /'
  echo "Remove them if you don't need them (rm, or sudo rm if they're not yours)."
fi
