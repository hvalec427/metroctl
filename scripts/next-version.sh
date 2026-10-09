#!/usr/bin/env bash
# Print the version the next stable release will get: one patch above the latest
# stable tag, or 0.1.0 if there is none. Stable, dev and nightly builds all use
# it, so dev and nightly carry the upcoming stable version.
set -euo pipefail

LATEST=$(git tag -l 'v[0-9]*' | grep -v -e '-' | sort -V | tail -1)
if [ -z "${LATEST}" ]; then
  echo "0.1.0"
  exit 0
fi
IFS=. read -r MAJOR MINOR PATCH <<< "${LATEST#v}"
echo "${MAJOR}.${MINOR}.$((PATCH + 1))"
