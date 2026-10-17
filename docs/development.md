# Building and contributing

## Build

Use a current stable Rust toolchain (MSVC target):

```
cargo build --release
```

This produces one program, `target\release\ferry.exe`.

- Started with no arguments, it is the engine and tray icon, which runs all the time.
- When you open Ferry from the tray, the engine starts a second copy of itself with `--flyout` or `--setup` for the window, and that copy exits when you close it. The window's renderer is only in memory while a window is open, so the part that runs all day stays small.

If the engine is already running from `target\release`, build with `--target-dir target\other` so the build doesn't try to replace the running program. `ferry.exe --where` prints the network adapters and where Ferry thinks you are, which helps when debugging location problems.

## The crates

- `crates/ferry-core`: pure logic (config, diff, location, delete rules, snapshots, IPC types). Unit-tested, no Win32.
- `crates/ferry`: the program (`ferry.exe`): engine, tray icon and copy code (Win32). It links `ferry-ui` but only calls it for `--flyout` and `--setup`. No tokio or other async runtime; check with `cargo tree -p ferry`.
- `crates/ferry-ui`: the flyout and setup window, built with Slint, as a library. The window runs in its own process and talks to the engine over a local named pipe.

## Before sending a change

```
cargo test --workspace
cargo clippy --workspace --all-targets
```

Look and wording rules for the UI are in [PRODUCT.md](../PRODUCT.md) and [DESIGN.md](../DESIGN.md). The code is the reference for behaviour, and [verification.md](verification.md) lists the checks and drills.

## Releases

Releases are built by CI (`.github/workflows/build.yml`). Every push to `main` and every pull request runs the tests on Windows and builds the program and the installer. Pushing a tag `vX.Y.Z` that matches the crate version publishes a release, with `SHA256SUMS.txt` signed as described in [updates.md](updates.md#who-can-ship-an-update).
