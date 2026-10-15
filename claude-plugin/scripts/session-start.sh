#!/bin/sh
# rn-sim SessionStart: in a React Native checkout, point boop at this
# checkout's signalbox session, give a new worktree the main checkout's
# dependencies, and tell the agent whether its app is running.
# Anywhere else it does nothing.

dir="${CLAUDE_PROJECT_DIR:-$PWD}"
root=$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null) || exit 0
[ -d "$root/ios" ] && grep -q '"react-native"' "$root/package.json" 2>/dev/null || exit 0

# boop drives the device of the nearest .signalbox/session.json, so it can only
# ever reach this checkout's simulator.
[ -n "$CLAUDE_ENV_FILE" ] && echo "export BOOP_DEVICE_FROM=.signalbox/session.json" >> "$CLAUDE_ENV_FILE"

# A new worktree has no node_modules, Pods or .env: clone them from the main
# checkout (APFS clones, instant, no extra space until a file changes).
main=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)
main=${main%/.git}
copied=""
if [ -n "$main" ] && [ "$main" != "$root" ]; then
  for rel in node_modules ios/Pods ios/.xcode.env.local $(cd "$main" && ls -d .env* 2>/dev/null); do
    if [ -e "$main/$rel" ] && [ ! -e "$root/$rel" ]; then
      mkdir -p "$(dirname "$root/$rel")"
      cp -cR "$main/$rel" "$root/$rel" 2>/dev/null && copied="$copied $rel"
    fi
  done
fi
[ -n "$copied" ] && echo "rn-sim cloned these from the main checkout into this worktree:$copied."

if ! command -v signalbox >/dev/null 2>&1; then
  echo "This is a React Native checkout, but signalbox isn't installed, so its app can't be run here (see https://github.com/hvalec427/signalbox)."
  exit 0
fi

session="$root/.signalbox/session.json"
pid=$(sed -n 's/.*"pid": *\([0-9]*\).*/\1/p' "$session" 2>/dev/null)
if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
  port=$(sed -n 's/.*"port": *\([0-9]*\).*/\1/p' "$session")
  echo "This React Native checkout's app is running under signalbox (Metro port $port), on a simulator of its own; signalbox's tools and boop already target it. The run-app skill covers using them."
else
  echo "This is a React Native checkout and its app isn't running. Answer questions from the code; before changing or checking anything you'll need to see in the app, use the run-app skill, which starts this checkout's own simulator and Metro."
fi
