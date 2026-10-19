# Roadmap

What's planned, roughly in order. Nothing here has a date.

## Next

- **Make the repository public.** While it is private, GitHub's releases API answers "not found", so no installed copy finds an update until it is public.
- **Check the server's identity.** Ferry recognises a place by its router and trusts the server that answers there (see [What Ferry trusts](how-it-works.md#what-ferry-trusts)). Pin each server's identity when it is first learned, and refuse a different one. Audit finding S1.
- **Sign the program.** Authenticode-sign `ferry.exe` and the installer, so Windows SmartScreen stops warning and a binary can be tied to its author. Audit finding S4. Once the repository is public, SignPath Foundation signs open-source projects built on GitHub Actions for free.

## Later

- **macOS and Linux.** The windows and the core logic already build for both; the engine (SMB access, location detection, credentials, tray, notifications, updates, packaging) needs a version per OS. The plan is in [macos-linux-port.md](macos-linux-port.md).
- **Path checks for more VPNs.** Today only Tailscale is checked; WireGuard, OpenVPN and ZeroTier need the trust switch.
- **Smaller fixes from the 2026-10-17 audit** ([audit](audit-2026-10-17.md)):
  - files hidden by the server (access-based enumeration, veto files) look deleted (D4);
  - a file that can never be carried holds its folder's deletes with no message (D5);
  - confirming deletes outside a trip checks only the count (D6);
  - Windows case folding for unusual letters (D7);
  - leftover `.ferry-part` files at the destination are never cleaned up (D9);
  - the cargo list is rewritten on every file, slow with 100,000 small files (D10);
  - a credential-conflict SMB session counts as signed in (S5).
- **Re-measure the footprint** on the current single-program build ([verification.md](verification.md)).
