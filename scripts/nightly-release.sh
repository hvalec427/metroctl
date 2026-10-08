#!/usr/bin/env bash
# Cut a nightly prerelease from develop. Runs once a day (scheduled at 23:00 UTC),
# tagged with the UTC date (e.g. 0.2.0-nightly.20261008) — one build per day.
set -euo pipefail

REPO="hvalec427/metroctl"

# Base = next minor above the latest stable, so nightlies sort ahead of stable.
# Authenticated: Actions runners share IPs and hit the anonymous rate limit (403).
LATEST=$(curl -fsSL -H "Authorization: Bearer ${GH_TOKEN}" -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/$REPO/releases/latest" \
  | grep '"tag_name"' | head -1 | cut -d'"' -f4 | sed 's/^v//')
if [ -n "${LATEST:-}" ]; then
  MAJOR=$(echo "$LATEST" | cut -d. -f1)
  MINOR=$(echo "$LATEST" | cut -d. -f2)
  BASE="${MAJOR}.$((MINOR + 1)).0"
else
  BASE="0.1.0"
fi

# Previous nightly = highest date suffix.
PREV=$(git tag -l 'v*-nightly.*' | sort -t. -k4,4 -n | tail -1)
[ -z "${PREV}" ] && PREV="${LATEST:+v$LATEST}"

# Skip when nothing that affects the binary changed since the last nightly.
if [ -n "${PREV}" ] && git rev-parse "${PREV}" >/dev/null 2>&1; then
  CODE_CHANGES=$(git diff --name-only "${PREV}" HEAD -- . ':(exclude)docs/**' ':(exclude)*.md' ':(exclude)LICENSE')
  if [ -z "${CODE_CHANGES}" ]; then
    echo "No code changes since ${PREV} — skipping nightly."
    exit 0
  fi
fi

TS=$(date -u +%Y%m%d)
VERSION="${BASE}-nightly.${TS}"
TAG="v${VERSION}"

echo "Building nightly ${TAG} (latest stable: ${LATEST:-none}, since ${PREV:-start})"
bash scripts/build-binaries.sh "${VERSION}"

RANGE="${PREV:+${PREV}..}HEAD"
NOTES=$(mktemp)
{
  echo "Automated nightly build from \`develop\`.${PREV:+ Changes since \`${PREV}\`:}"
  echo
  git log ${RANGE} --no-merges --pretty=format:'- %s (%h)' 2>/dev/null || true
  echo
  echo
  echo "### Install this build"
  echo
  echo '```sh'
  echo "curl -fsSL https://github.com/${REPO}/releases/download/${TAG}/metroctl-darwin-arm64 -o metroctl \\"
  echo "  && chmod +x metroctl && sudo mv metroctl /usr/local/bin/metroctl"
  echo '```'
  echo
  echo "_Apple Silicon shown; on Intel use \`metroctl-darwin-x64\`. Already installed? \`metroctl update --nightly\`._"
} > "${NOTES}"

# --target the built develop commit so the tag points at what we built.
gh release create "${TAG}" \
  --prerelease \
  --target "$(git rev-parse HEAD)" \
  --title "${TAG}" \
  --notes-file "${NOTES}" \
  metroctl-darwin-arm64 metroctl-darwin-x64
