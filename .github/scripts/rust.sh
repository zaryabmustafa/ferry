#!/usr/bin/env bash
# Install rustup if needed and the toolchain pinned in rust-toolchain.toml (with its components
# and the Windows GNU target). With ~/.rustup restored from cache this downloads nothing.
#
# TLS handshakes to Rust's CDN (Fastly) can stall intermittently: the same file times
# out once and downloads in seconds on the next try. So every download here uses a short
# connect timeout and many retries; the sh.rustup.rs bootstrap is skipped because its own curl
# call has no timeout and can hang for the job's whole time limit.
set -euo pipefail

retry() {
  local n=1
  until "$@"; do
    ((n >= 8)) && { echo "::error::giving up after $n attempts: $*"; return 1; }
    echo "attempt $n failed, retrying in $((n * 3))s"; sleep $((n * 3)); ((n++))
  done
}

if ! command -v rustup >/dev/null 2>&1; then
  if [[ ! -x "$HOME/.cargo/bin/rustup" ]]; then
    url=https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init
    retry curl -4 -sSfL --connect-timeout 10 --max-time 180 "$url" -o /tmp/rustup-init
    chmod +x /tmp/rustup-init
    retry /tmp/rustup-init -y --profile minimal --default-toolchain none --no-modify-path
  fi
  export PATH="$HOME/.cargo/bin:$PATH"
  echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"
fi

# Reads rust-toolchain.toml in the working directory.
retry rustup toolchain install
rustup show active-toolchain
rustc --version
cargo --version

# Download every crate up front, with retries, so the build steps don't touch the network.
retry cargo fetch --locked --target "${TARGET:-x86_64-pc-windows-gnu}"
