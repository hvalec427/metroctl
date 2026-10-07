#!/bin/sh
set -e

TARGET=$(command -v metroctl 2>/dev/null || true)
if [ -z "$TARGET" ]; then
  echo "metroctl is not on your PATH."
  exit 0
fi

DIR=$(dirname "$TARGET")
if [ -w "$DIR" ]; then
  rm "$TARGET"
else
  sudo rm "$TARGET"
fi
echo "metroctl uninstalled from $TARGET"

# A second copy may still be on PATH — flag it so uninstall is actually complete.
NEXT=$(command -v metroctl 2>/dev/null || true)
if [ -n "$NEXT" ]; then
  echo "Note: another copy remains at $NEXT — run this again to remove it."
fi
