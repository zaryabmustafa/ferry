#!/usr/bin/env bash
# Create (or reuse) the GitHub release for $TAG with its notes, and upload every file in the given
# directory, replacing assets of the same name, so re-running a tag's workflow is safe.
# Needs: GH_TOKEN, GH_REPO (<owner>/<repo>), URL (…/<owner>/<repo>), TAG.
# Usage: release.sh [dir]   (no dir: only write the notes of an existing release)
set -euo pipefail
dir=${1:-}
version=${TAG#v}
here=$(dirname "$0")

notes="Windows 10/11 (x64). Install with Ferry-Setup-$version.exe (no admin rights needed), or unzip the portable build and run ferry.exe. Checksums are in SHA256SUMS.txt, signed in SHA256SUMS.txt.sig (ECDSA P-256; the public key is UPDATE_KEY in crates/ferry/src/update.rs).

<a href=\"https://slint.dev\"><img src=\"https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png\" alt=\"Made with Slint\" width=\"160\"></a>

$(bash "$here/changelog.sh" "$TAG")"

if gh release view "$TAG" >/dev/null 2>&1; then
  [[ -n "$dir" ]] || { gh release edit "$TAG" --notes "$notes" >/dev/null; echo "Release $TAG has its notes"; exit 0; }
  # Replacing files: back to a draft until all of them are up, so an updater never sees a
  # SHA256SUMS.txt that doesn't match the installer beside it.
  gh release edit "$TAG" --draft --notes "$notes" >/dev/null
  gh release upload "$TAG" "$dir"/* --clobber
  gh release edit "$TAG" --draft=false >/dev/null
elif [[ -n "$dir" ]]; then
  # With files, gh creates the release as a draft, uploads them, and only then publishes it, so
  # installed copies never see a release without its SHA256SUMS.txt.sig.
  pre=()
  [[ "$TAG" == *-* ]] && pre=(--prerelease)
  gh release create "$TAG" "$dir"/* --verify-tag --title "Ferry $version" --notes "$notes" "${pre[@]}"
else
  echo "No release for $TAG; skipped"
  exit 0
fi
gh release view "$TAG" --json assets --jq '.assets[].name'
