#!/usr/bin/env bash
# Create (or reuse) the Gitea release for $TAG with its notes, and upload every file in the given
# directory, replacing assets of the same name, so re-running a tag's workflow is safe.
# Needs: TOKEN, API (…/api/v1/repos/<owner>/<repo>), URL (…/<owner>/<repo>), TAG.
# Usage: release.sh [dir]   (no dir: only write the notes of an existing release)
set -euo pipefail
dir=${1:-}
auth=(-H "Authorization: token $TOKEN")
version=${TAG#v}
here=$(dirname "$0")

notes="Windows 10/11 (x64). Install with Ferry-Setup-$version.exe (no admin rights needed), or unzip the portable build and run ferry.exe. Checksums are in SHA256SUMS.txt, signed in SHA256SUMS.txt.sig (ECDSA P-256; the public key is UPDATE_KEY in crates/ferry/src/update.rs).

<a href=\"https://slint.dev\"><img src=\"https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png\" alt=\"Made with Slint\" width=\"160\"></a>

$(bash "$here/changelog.sh" "$TAG")"

id=$(curl -sf "${auth[@]}" "$API/releases/tags/$TAG" | jq -r '.id // empty' || true)
if [[ -z "$id" ]]; then
  [[ -n "$dir" ]] || { echo "No release for $TAG; skipped"; exit 0; }
  body=$(jq -n --arg tag "$TAG" --arg name "Ferry $version" --arg body "$notes" \
    '{tag_name: $tag, name: $name, body: $body, draft: false, prerelease: ($tag | test("-"))}')
  id=$(curl -sf "${auth[@]}" -H "Content-Type: application/json" -d "$body" "$API/releases" | jq -r '.id')
else
  jq -n --arg body "$notes" '{body: $body}' |
    curl -sf "${auth[@]}" -X PATCH -H "Content-Type: application/json" -d @- "$API/releases/$id" >/dev/null
fi
[[ -n "$id" && "$id" != null ]] || { echo "::error::Could not create the release for $TAG"; exit 1; }
echo "Release $TAG has id $id and its notes"

[[ -n "$dir" ]] || exit 0
existing=$(curl -sf "${auth[@]}" "$API/releases/$id/assets" | jq -r '.[] | "\(.id) \(.name)"')
for f in "$dir"/*; do
  name=$(basename "$f")
  old=$(awk -v n="$name" '$2 == n {print $1}' <<<"$existing")
  [[ -n "$old" ]] && curl -sf "${auth[@]}" -X DELETE "$API/releases/$id/assets/$old"
  curl -sf "${auth[@]}" -F "attachment=@$f" "$API/releases/$id/assets?name=$name" >/dev/null
  echo "Uploaded $name"
done
