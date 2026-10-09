#!/bin/sh
set -e

REPO="hvalec427/metroctl"

# Detect architecture
ARCH=$(uname -m)
if [ "$ARCH" = "arm64" ]; then
  FILE="metroctl-darwin-arm64"
else
  FILE="metroctl-darwin-x64"
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

# The channel `metroctl update` follows from now on, recorded below.
case "$VERSION" in
  dev) CHANNEL="dev" ;;
  *-dev.*) CHANNEL="dev" ;;
  *-nightly.*) CHANNEL="nightly" ;;
  *) CHANNEL="stable" ;;
esac

# Install over the metroctl already on PATH if there is one, so we never leave a
# stale copy shadowing the new version; otherwise default to /usr/local/bin.
EXISTING=$(command -v metroctl 2>/dev/null || true)
if [ -n "$EXISTING" ]; then
  INSTALL_PATH="$EXISTING"
else
  INSTALL_PATH="/usr/local/bin/metroctl"
fi
INSTALL_DIR=$(dirname "$INSTALL_PATH")

URL="https://github.com/$REPO/releases/download/$VERSION/$FILE"

echo "Installing metroctl $VERSION ($ARCH) to $INSTALL_PATH..."
curl -fsSL "$URL" -o /tmp/metroctl
chmod +x /tmp/metroctl
# Strip the macOS quarantine flag so Gatekeeper doesn't block the (un-notarized)
# binary with "Apple could not verify ... free of malware". No-op off macOS.
xattr -d com.apple.quarantine /tmp/metroctl 2>/dev/null || true

# Only use sudo when the target directory isn't writable.
if [ -w "$INSTALL_DIR" ]; then
  mv /tmp/metroctl "$INSTALL_PATH"
else
  sudo mv /tmp/metroctl "$INSTALL_PATH"
fi

# Remember the channel so a plain `metroctl update` stays on it.
CONFIG_DIR="$HOME/.config/metroctl"
mkdir -p "$CONFIG_DIR"
printf '{\n  "channel": "%s"\n}\n' "$CHANNEL" > "$CONFIG_DIR/update.json"

echo "Done — metroctl $VERSION ($CHANNEL channel) installed to $INSTALL_PATH"

# Warn if some other metroctl earlier in PATH would still win.
RESOLVED=$(command -v metroctl 2>/dev/null || true)
if [ -n "$RESOLVED" ] && [ "$RESOLVED" != "$INSTALL_PATH" ]; then
  echo "Warning: 'metroctl' still resolves to $RESOLVED, which shadows the new install."
  echo "Remove that copy or fix your PATH, then run: hash -r"
fi
