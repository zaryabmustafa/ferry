# Roadmap

What's planned, roughly in order. Nothing here has a date.

## Next

- **Make the repository public.** While it is private, GitHub's releases API answers "not found", so no installed copy finds an update until it is public.
- **Sign the program.** Authenticode-sign `ferry.exe` and the installer, so Windows SmartScreen stops warning and a binary can be tied to its author. Audit finding S4. Once the repository is public, SignPath Foundation signs open-source projects built on GitHub Actions for free.

## Later

- **macOS and Linux.** The windows and the core logic already build for both; the engine (SMB access, location detection, credentials, tray, notifications, updates, packaging) needs a version per OS. The plan is in [macos-linux-port.md](macos-linux-port.md).
- **Path checks for more VPNs.** Today only Tailscale is checked; WireGuard, OpenVPN and ZeroTier need the trust switch.
- **Re-measure the footprint** on the current single-program build ([verification.md](verification.md)).
