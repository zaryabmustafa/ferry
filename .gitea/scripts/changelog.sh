#!/usr/bin/env bash
# Print the release notes for a tag: its commits since the previous tag, grouped by the
# conventional-commit type in their subject ("feat(ui): ..." goes under New).
# Needs full history and tags (actions/checkout with fetch-depth: 0).
# Needs: URL (…/<owner>/<repo>, for the compare link).
# Usage: changelog.sh <tag>
set -euo pipefail
tag=$1
prev=$(git describe --tags --abbrev=0 --match 'v*' "$tag^" 2>/dev/null || true)

echo "## What's changed"
echo
if [[ -z "$prev" ]]; then
  echo "First release."
  exit 0
fi

declare -A group
order=(New Fixed Documentation Build Other)
while IFS= read -r s; do
  [[ -z "$s" ]] && continue
  head=${s%%:*}
  kind=other
  if [[ "$s" == *": "* && "$head" != *" "* ]]; then
    kind=${head%%(*}
    s=${s#*: }
  fi
  case $kind in
    feat) g=New ;;
    fix) g=Fixed ;;
    docs) g=Documentation ;;
    ci|build) g=Build ;;
    release|chore) continue ;;
    *) g=Other ;;
  esac
  group[$g]+="- ${s^}"$'\n'
done < <(git log --no-merges --format=%s "$prev..$tag")

any=false
for g in "${order[@]}"; do
  [[ -n "${group[$g]:-}" ]] || continue
  any=true
  echo "### $g"
  echo
  printf '%s\n' "${group[$g]}"
done
$any || { echo "No changes besides the version number."; echo; }
echo "All commits: [$prev...$tag]($URL/compare/$prev...$tag)"
