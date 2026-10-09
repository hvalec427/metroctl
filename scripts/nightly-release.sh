#!/usr/bin/env bash
# Cut a nightly prerelease from develop. Runs once a day (scheduled at 21:00 UTC),
# tagged with the UTC date (e.g. 0.2.0-nightly.20261008) — one build per day.
set -euo pipefail

REPO="hvalec427/metroctl"

# Base = the version the next stable release will get; LATEST is for notes.
# Authenticated: Actions runners share IPs and hit the anonymous rate limit (403).
LATEST=$(curl -fsSL -H "Authorization: Bearer ${GH_TOKEN}" -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/$REPO/releases/latest" \
  | grep '"tag_name"' | head -1 | cut -d'"' -f4 | sed 's/^v//')
BASE=$(bash scripts/next-version.sh)

# Previous nightly = highest date suffix.
TS=$(date -u +%Y%m%d)
TAG="v${BASE}-nightly.${TS}"
# Sort on the 8-digit date prefix first: older nightlies used 14-digit
# timestamps, which would otherwise always sort above plain dates.
PREV=$(git tag -l 'v*-nightly.*' | grep -vx -e "${TAG}" | sort -t. -k4.1,4.8n -k4,4n | tail -1)
[ -z "${PREV}" ] && PREV="${LATEST:+v$LATEST}"

# Skip when nothing that affects the binary changed since the last nightly.
SINCE="${PREV}"
git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null && SINCE="${TAG}"
if [ -n "${SINCE}" ] && git rev-parse "${SINCE}" >/dev/null 2>&1; then
  CODE_CHANGES=$(git diff --name-only "${SINCE}" HEAD -- . ':(exclude)docs/**' ':(exclude)*.md' ':(exclude)LICENSE')
  if [ -z "${CODE_CHANGES}" ]; then
    echo "No code changes since ${SINCE} — skipping nightly."
    exit 0
  fi
fi

VERSION="${BASE}-nightly.${TS}"

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

# A nightly already cut today is replaced, so the day's last build wins.
if gh release view "${TAG}" --repo "$REPO" >/dev/null 2>&1; then
  gh release delete "${TAG}" --repo "$REPO" --cleanup-tag --yes
fi

# --target the built develop commit so the tag points at what we built.
gh release create "${TAG}" \
  --prerelease \
  --target "$(git rev-parse HEAD)" \
  --title "${TAG}" \
  --notes-file "${NOTES}" \
  metroctl-darwin-arm64 metroctl-darwin-x64
