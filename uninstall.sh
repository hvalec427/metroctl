#!/bin/sh
# Remove metroctl from ~/.metroctl/bin and its PATH line.
set -e

TOOL="metroctl"
BIN_DIR="$HOME/.$TOOL/bin"

if [ -f "$BIN_DIR/$TOOL" ]; then
  rm -f "$BIN_DIR/$TOOL"
  rmdir "$BIN_DIR" 2>/dev/null || true
  rmdir "$(dirname "$BIN_DIR")" 2>/dev/null || true
  echo "$TOOL uninstalled from $BIN_DIR"
else
  echo "$TOOL isn't installed in $BIN_DIR."
fi

# The PATH line the installer added (and its "# $TOOL" comment).
case "$BIN_DIR" in
  "$HOME"/*) PATH_DIR="\$HOME${BIN_DIR#"$HOME"}" ;;
  *) PATH_DIR="$BIN_DIR" ;;
esac
for RC in "${ZDOTDIR:-$HOME}/.zshrc" "$HOME/.bash_profile" "$HOME/.bashrc" "$HOME/.profile" "$HOME/.config/fish/config.fish"; do
  if grep -qsF "$PATH_DIR" "$RC"; then
    TMP="$(mktemp)"
    grep -vF "$PATH_DIR" "$RC" | sed "/^# $TOOL\$/d" > "$TMP"
    cat "$TMP" > "$RC" && rm -f "$TMP"
    echo "Removed $PATH_DIR from PATH in $RC"
  fi
done

# Copies elsewhere (e.g. an older install in /usr/local/bin).
OTHER=$(which -a "$TOOL" 2>/dev/null | grep -vxF "$BIN_DIR/$TOOL" | sort -u || true)
if [ -n "$OTHER" ]; then
  echo "Other copies of $TOOL are still on your PATH:"
  echo "$OTHER" | sed 's/^/  /'
fi
