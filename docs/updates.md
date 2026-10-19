# Updates

A copy installed with `Ferry-Setup-<version>.exe` keeps itself up to date. The portable `ferry.exe` and copies built from source don't update themselves; Settings tells you when a newer version is out.

## What happens

1. **About once a day** Ferry asks the repository's releases API whether there is a newer release. It doesn't ask on a metered connection (a phone hotspot or metered Wi-Fi), while Ferry is paused, while a trip is running, or while a Ferry window is open.
2. **When there is one,** it checks that the release's `SHA256SUMS.txt` is signed with Ferry's update key, downloads the installer (about 5 MB) to `%LOCALAPPDATA%\Ferry\update`, and checks it against `SHA256SUMS.txt`.
3. **It installs silently and starts again.** A notification says "Ferry updated to X.Y.Z" and the activity log has a line for it.

Nothing else is downloaded, and your files never travel this way.

## In Settings

- **Settings > Updates** shows the version you have.
- **Check for updates** asks right away, metered or not.
- **Update to X.Y.Z** appears when a newer version exists. It installs the same silent way; Ferry closes the window and starts again.
- **Install updates automatically** is on by default. Turn it off if you'd rather update by hand.

## When an update is refused

| What went wrong | What Ferry does |
|---|---|
| `SHA256SUMS.txt.sig` is missing, or isn't the update key's signature | Downloads nothing. Activity says "update X.Y.Z isn't signed with Ferry's update key, not installed". |
| The installer fails its checksum | Deletes the download and doesn't install it. |
| The install doesn't take | Starts the old version again. |

In each case that version is skipped until a newer one comes out. Ferry also only follows installer and checksum links that use https on the same host as the releases API (or on `github.com` for `api.github.com`, where GitHub serves release files), and never installs a version older than the one it runs.

## Who can ship an update

The signature is ECDSA P-256 over the exact bytes of `SHA256SUMS.txt`, which names the installer and its version. The public key is built into `ferry.exe` (`UPDATE_KEY` in `crates/ferry/src/update.rs`). CI signs with the private key, which lives only in the repository's `UPDATE_SIGNING_KEY` Actions secret and the owner's offline copy.

- **Can't ship an update:** someone who can only upload or replace release files, for example with a stolen token that can edit releases.
- **Still can:** someone who can run a workflow that reads that secret (write access to the repository, or control of the owner's GitHub account). To take that away too, sign by hand instead of in CI: see "Update signing key" in [verification.md](verification.md#update-signing-key).

Turn off "Install updates automatically" if this is more trust than you want to give it.

## Private repositories and forks

While the repository is private, the releases API answers "not found" to Ferry, so Ferry sees no updates. That is quiet, and updates start working once the repository is public.

A fork points Ferry at its own releases by building with `FERRY_RELEASES_URL` set to its `https://api.github.com/repos/<owner>/<repo>/releases/latest` address, and at its own signing key with `FERRY_UPDATE_KEY` (or by changing `UPDATE_KEY`). A build without a key installs no updates.
