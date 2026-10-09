#!/usr/bin/env bash
# Conventional-commit check — the @commitlint/config-conventional rules that
# matter: `type(scope)!: subject`, a known type, header ≤ 100 chars. Git's own
# merge/revert/fixup messages pass, as in commitlint.
#   commit-lint.sh --file <msg-file>   one message (commit-msg hook)
#   commit-lint.sh <from>..<to>        every non-merge commit in a range (CI)
set -euo pipefail

TYPES='build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test'
RE="^(${TYPES})(\([^)]+\))?!?: [^ ]"

valid() {
  case "$1" in
    "Merge "* | "Revert \""* | "fixup! "* | "squash! "* | "amend! "*) return 0 ;;
  esac
  [[ "$1" =~ $RE ]] && [ "${#1}" -le 100 ]
}

explain() {
  echo "Commit messages must be conventional: <type>(<scope>)?: <subject>" >&2
  echo "  types: ${TYPES//|/, }  — e.g. \"feat(rn): add device picker\"" >&2
}

if [ "${1:-}" = "--file" ]; then
  HEADER=$(grep -v '^#' "$2" | head -1)
  if ! valid "${HEADER}"; then
    echo "✖ Not a conventional commit: ${HEADER}" >&2
    explain
    exit 1
  fi
  exit 0
fi

RANGE="$1"
# A new branch (before = 000…) or a rewritten one: just check the tip.
FROM="${RANGE%%..*}"
if [[ "${FROM}" =~ ^0+$ ]] || ! git cat-file -e "${FROM}^{commit}" 2>/dev/null; then
  RANGE="${RANGE##*..}~1..${RANGE##*..}"
fi

BAD=0
while IFS=$'\t' read -r sha subject; do
  if ! valid "${subject}"; then
    echo "✖ ${sha} ${subject}" >&2
    BAD=1
  fi
done < <(git log "${RANGE}" --no-merges --format='%h%x09%s')

if [ "${BAD}" -ne 0 ]; then
  explain
  exit 1
fi
echo "All commits in ${RANGE} are conventional."
