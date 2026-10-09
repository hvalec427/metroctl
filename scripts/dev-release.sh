#!/usr/bin/env bash
# Build a "dev" build from the current develop commit and publish it to a single
# rolling prerelease tagged `dev` (binaries overwritten each push). Install with
# `install.sh dev`. The version lives in the release title.
set -euo pipefail

REPO="hvalec427/metroctl"

# Base = the version the next stable release will get.
BASE=$(bash scripts/next-version.sh)

TS=$(date -u +%Y%m%d%H%M%S)
VERSION="${BASE}-dev.${TS}"

echo "Building dev ${VERSION} ($(git rev-parse --short HEAD))"
bash scripts/build-binaries.sh "${VERSION}"

NOTES="Rolling dev build — the latest \`develop\` commit, rebuilt on every push. Install with \`install.sh dev\`."

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
