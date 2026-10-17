#!/usr/bin/env bash
# Prepare an ubuntu-latest runner: prefer IPv4 (some runners time out reaching Rust's CDN over
# IPv6) and install the given apt packages, retrying transient mirror failures.
# Usage: prepare.sh <apt package>...
set -euo pipefail

SUDO=$(command -v sudo || true)
grep -q '^precedence ::ffff:0:0/96' /etc/gai.conf 2>/dev/null \
  || echo 'precedence ::ffff:0:0/96  100' | $SUDO tee -a /etc/gai.conf >/dev/null

missing=()
for pkg in "$@"; do
  dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q 'install ok installed' || missing+=("$pkg")
done
if ((${#missing[@]})); then
  for attempt in 1 2 3; do
    if $SUDO apt-get update -qq && $SUDO apt-get install -y -qq --no-install-recommends "${missing[@]}" >/dev/null; then
      break
    fi
    [[ $attempt == 3 ]] && { echo "::error::apt-get failed for: ${missing[*]}"; exit 1; }
    echo "apt-get attempt $attempt failed, retrying"; sleep 10
  done
fi
echo "Ready: $*"
