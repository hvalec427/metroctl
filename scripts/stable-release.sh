#!/usr/bin/env bash
# Cut a stable release from the pushed master commit, versioned from the
# conventional commits since the last one (scripts/next-version.sh). Skips if
# this commit is already released.
set -euo pipefail

REPO="hvalec427/metroctl"

LATEST=$(git tag -l 'v*' | grep -v -e '-' | sort -V | tail -1 | sed 's/^v//')
if [ -n "${LATEST:-}" ]; then
  if [ "$(git rev-list -n1 "v${LATEST}")" = "$(git rev-parse HEAD)" ]; then
    echo "HEAD is already released as v${LATEST} — skipping."
    exit 0
  fi
fi
VERSION=$(bash scripts/next-version.sh)
TAG="v${VERSION}"

echo "Building stable ${TAG} (previous: ${LATEST:-none})"
bash scripts/build-binaries.sh "${VERSION}"

NOTES=$(mktemp)
{
  echo "metroctl ${VERSION}"
  echo
  git log ${LATEST:+v${LATEST}..}HEAD --no-merges --pretty=format:'- %s (%h)' 2>/dev/null || true
} > "${NOTES}"

gh release create "${TAG}" \
  --target "$(git rev-parse HEAD)" \
  --title "${TAG}" \
  --notes-file "${NOTES}" \
  metroctl-darwin-arm64 metroctl-darwin-x64
