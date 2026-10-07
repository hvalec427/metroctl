#!/usr/bin/env bash
# Build a "dev" build from the current main commit and publish it to a single
# rolling prerelease tagged `dev` (binaries overwritten each push). Install with
# `install.sh dev`. The version lives in the release title.
set -euo pipefail

REPO="hvalec427/metroctl"

# Base = next patch above the latest stable tag, or 0.1.0 if there is none yet.
LATEST=$(gh release list --repo "$REPO" --exclude-pre-releases --limit 1 2>/dev/null | awk 'NR==1{print $1}' | sed 's/^v//')
if [ -n "${LATEST:-}" ]; then
  MAJOR=$(echo "$LATEST" | cut -d. -f1)
  MINOR=$(echo "$LATEST" | cut -d. -f2)
  PATCH=$(echo "$LATEST" | cut -d. -f3)
  BASE="${MAJOR}.${MINOR}.$((PATCH + 1))"
else
  BASE="0.1.0"
fi

TS=$(date -u +%Y%m%d%H%M%S)
VERSION="${BASE}-dev.${TS}"

echo "Building dev ${VERSION} ($(git rev-parse --short HEAD))"
bash scripts/build-binaries.sh "${VERSION}"

NOTES="Rolling dev build — the latest \`main\` commit, rebuilt on every push. Install with \`install.sh dev\`."

if gh release view dev --repo "$REPO" >/dev/null 2>&1; then
  gh release edit dev --repo "$REPO" --title "${VERSION}" --prerelease --notes "${NOTES}"
  gh release upload dev --repo "$REPO" metroctl-darwin-arm64 metroctl-darwin-x64 --clobber
else
  gh release create dev --repo "$REPO" \
    --prerelease \
    --target "$(git rev-parse HEAD)" \
    --title "${VERSION}" \
    --notes "${NOTES}" \
    metroctl-darwin-arm64 metroctl-darwin-x64
fi

echo "Published dev ${VERSION} to the rolling 'dev' release."
