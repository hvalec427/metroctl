#!/bin/sh
# rn-sim SessionEnd: stop the app an agent started in this checkout (signalbox
# up --detach), which deletes the simulator it created. A dashboard you
# started yourself is left alone.

dir="${CLAUDE_PROJECT_DIR:-$PWD}"
root=$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null) || exit 0
[ -f "$root/.signalbox/session.json" ] || exit 0
command -v signalbox >/dev/null 2>&1 || exit 0
cd "$root" && signalbox down --detached-only >/dev/null 2>&1
exit 0
