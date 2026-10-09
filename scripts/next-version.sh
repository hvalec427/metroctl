#!/usr/bin/env bash
# Print the version the next stable release will get, from the conventional
# commits since the latest stable tag — the same rules semantic-release applies:
# a breaking change bumps major, a feat bumps minor, anything else bumps patch.
# Dev and nightly builds use it as their base, so they carry the upcoming version.
set -euo pipefail

LATEST=$(git tag -l 'v[0-9]*' | { grep -v -e '-' || true; } | sort -V | tail -1)
if [ -z "${LATEST}" ]; then
  echo "0.1.0"
  exit 0
fi
IFS=. read -r MAJOR MINOR PATCH <<< "${LATEST#v}"

LOG=$(git log "${LATEST}..HEAD" --no-merges --format='%s%n%b')
if grep -qE '^[a-z]+(\([^)]*\))?!:|^BREAKING[ -]CHANGE' <<< "${LOG}"; then
  echo "$((MAJOR + 1)).0.0"
elif grep -qE '^feat(\([^)]*\))?:' <<< "${LOG}"; then
  echo "${MAJOR}.$((MINOR + 1)).0"
else
  echo "${MAJOR}.${MINOR}.$((PATCH + 1))"
fi
