# Ferry verification

Measured on 2026-10-16 on the development laptop (Windows 11). Rows 1, 2, 3, 13 and 14 are from a fresh release build, tests and clippy of the v1 code at commit 8ada633. Rows 4 to 6 are from an engine already running on the laptop, which is an older build of the same v1 code (see the notes under the footprint table). Nothing has been re-measured on the v2 code (VPN-agnostic location, Tailscale path check, sneakernet mode). Rows marked "Not yet run" need the real source and destination servers, an elevated prompt, or a rebuild, and are a checklist for the user further down.

## Results

| # | Metric | Target | Measured | Result |
|---|--------|--------|----------|--------|
| 1 | `ferry.exe` size | no target | 14,988,288 bytes (14.3 MiB, MSVC release build of 0.2.0 plus the 2026-10-19 audit fixes; one program with the windows built in) | Info |
| 2 | `ferry-ui.exe` size | no target | Retired: since the single-program change the windows are part of `ferry.exe` (row 1). v1 measured 12,908,544 bytes | Retired |
| 3 | `slint`, `winit`, `tokio` in `cargo tree -p ferry` | none | Retired: `ferry.exe` now links Slint for its windows. CI still checks that `ferry-core` has no UI toolkit | Retired |
| 4 | Engine private bytes, idle (older build) | under 5 MB | 4,374,528 bytes (4.17 MiB), identical in all six samples | Pass for the older v1 build; not yet run on v2 |
| 5 | Engine working set, idle (older build) | under 5 MB | 2.49 to 3.05 MB over 13.5 minutes | Pass for the older v1 build; not yet run on v2 |
| 6 | Engine CPU time across 5 minutes idle (older build) | unchanged | +0.12 s over 5 min 42 s, +0.33 s over 13.5 min (about 0.04% of one core), not flat | Inconclusive (near zero), pending step A (powercfg); not yet run on v2 |
| 7 | Wakeups (`powercfg /energy`) | `ferry.exe` not listed as a wakeup or timer-resolution source | Needs an elevated prompt | Not yet run (user step A) |
| 8 | Tailscale-specific: fill at the source of 5 GB or more | Tailscale adapter counters rise by about the copied amount; router WAN (internet uplink) counters do not | Needs real servers and a router with WAN counters | Not yet run (user step B) |
| 9 | Drill (a): unplug mid-fill, next visit resumes | same `.ferry-part` resumes | Needs real servers | Not yet run (user step C) |
| 10 | Drill (b): flipped byte in cargo | damaged laptop copy dropped (no toast), destination unchanged, next fill carries it again | Needs real servers | Not yet run (user step D) |
| 11 | Drill (c): source share folder renamed | no deletes at the destination | Needs real servers | Not yet run (user step E) |
| 12 | Drill (d): 1 GB cap, 1.38 GB zip | one Oversize toast | Needs real servers | Not yet run (user step F) |
| 13 | `cargo test --workspace` | all pass | 141 passed, 0 failed on v1 (ferry 64, ferry-core 52, ferry-ui 25, doc-tests 0) | Pass on v1; not yet run on v2 |
| 14 | `cargo clippy --workspace --all-targets` | no warnings | 0 warnings, 0 errors | Pass on v1; not yet run on v2 |
| 15 | Path status in Locations on a real network | "Direct on this network", or "Through `<VPN>`, direct on this network" | Needs real servers | Not yet run (prerequisite) |
| 16 | Drill (e): sneakernet, no remote addresses | one destination visit, one fill at the source and one unload mirror a test folder, including one delete | Needs real servers | Not yet run (user step G) |
| 17 | Auto-update, local build 0.2.90 to 0.2.91 | manual and automatic update restart Ferry as the new version with a notification; a tampered download is refused; a 404 is quiet | Replaces the installed Ferry, so the owner runs it | Not yet run (user step H) |

Build (v1 code, 8ada633): `cargo build --release --target-dir target/verify` finished clean in 4 min 12 s from scratch with no warnings. The separate target dir was used because `target\release` holds the running binaries. The `target/verify` dir was deleted afterwards.

### Idle footprint detail

The engine measured is the one already running on the laptop (single-instance, so a second copy cannot be started). It was read with `Get-Process` only, nothing was stopped or restarted.

| Time | Working set | Private bytes | CPU seconds | Threads / handles |
|------|-------------|---------------|-------------|-------------------|
| 21:15:09 | 2,486,272 | 4,374,528 | 1.72 | n/a |
| 21:20:51 | 2,809,856 | 4,374,528 | 1.84 | n/a |
| 21:24:25 | 2,953,216 | 4,374,528 | 1.98 | 2 / 331 |
| 21:25:19 | 2,969,600 | 4,374,528 | 1.98 | n/a |
| 21:26:28 | 2,981,888 | 4,374,528 | 2.03 | n/a |
| 21:28:44 | 3,051,520 | 4,374,528 | 2.05 | n/a |

Notes:

- This `ferry.exe` was built at 20:11 and is older than the v1 commit 8ada633. Commits de1fe99 (20:41) and 7a3af2f (20:56) changed `crates/ferry/src/engine.rs` after it. The size (1,070,080 bytes) differs from the fresh `HEAD` build (1,082,880 bytes). Footprint should be re-read once after the user next restarts onto a current v2 build.
- CPU time did move by a few hundred milliseconds. The cause was not traced; the growth is small and comes in bursts (nothing for a minute, then 0.05 s), which fits a periodic timer and not a busy loop. If you want a stricter reading, sample `CPU` again 5 minutes apart with the laptop untouched; a rise of more than about 0.5 s per 5 minutes would be worth a look.
- These figures belong to the engine only. The tray UI is a separate process (`ferry-ui.exe` in v1; since 0.2.1, `ferry.exe --flyout` / `--setup`) and is not part of the 5 MB target; it was not sampled over time, so no figure is claimed for it.

## User checklist

Nothing below has been run. Each step says what to do and what should happen.

### Prerequisite: check the path status

Ferry only copies when it can tell the path to the server stays on the network the laptop is on. Check this before any drill that copies files.

1. At the source, open Ferry from the tray, go to Locations, and look at the source server's path status. At the destination, look at the destination server's.
2. Expected: **Direct on this network**, or **Through Tailscale, direct on this network** (or another VPN name if Ferry has a check for it). Both allow copies.
3. If it says "Through `<VPN>`: can't confirm it stays local, so Ferry won't copy files here", Ferry will not copy. Either change the VPN setting so it does not capture the local range, or, only if you know the VPN keeps this traffic on the local network, turn on "Trust this VPN for copies" for that learned network. Activity then notes each trip that relied on it.
4. To see which interface Windows uses, replace `192.0.2.10` with the server's LAN address:

   ```powershell
   (Find-NetRoute -RemoteIPAddress 192.0.2.10 | Where-Object { $_.InterfaceAlias }).InterfaceAlias | Select-Object -Unique
   ```

   Expected: `Ethernet` or `Wi-Fi` for a direct route. A VPN adapter name (for example `Tailscale`) means the route goes through the VPN, and the path status above must then say the link is direct.

### A. Wakeups (`powercfg /energy`, needs an elevated prompt)

1. Open PowerShell as administrator, with Ferry idle:

   ```powershell
   powercfg /energy /duration 60 /output "$env:TEMP\ferry-energy.html"
   ```

2. Open the HTML report and search for `ferry`.
3. Expected: `ferry.exe` is not named as a source of timer-resolution requests or wakeups. If the report lists it, record what it says; that also bears on the small CPU growth in row 6.

### B. Tailscale byte check (only if Tailscale carries the route to the server)

This drill only applies when the path status says "Through Tailscale, direct on this network", for example when the source LAN range is also advertised as a Tailscale subnet route and Windows sends it into Tailscale. If the route is plain Ethernet or Wi-Fi, skip it.

With that setup, file bytes at the source go through the Tailscale adapter but stay on the source LAN between the laptop and the server. So the check is: at the source the Tailscale adapter counters rise by the copied amount, while the internet uplink does not.

Pause other Tailscale use during the trip (remote desktop, file shares, backups through the tailnet), because the counters include everything on the adapter.

1. Confirm the peer is reached directly. In a terminal, with the peer's Tailscale IP:

   ```powershell
   tailscale ping -c 3 <peer-tailscale-ip>
   ```

   Expected: `pong from ... via 192.0.2.10:41641` (an address on the source LAN), not `via DERP(...)` and not a public address.
2. Before the trip, note the Tailscale adapter counters:

   ```powershell
   $a = Get-NetAdapterStatistics -Name Tailscale | Select-Object ReceivedBytes, SentBytes
   $a
   ```

   Also note the internet uplink counters from the router's WAN interface (its status page or API). Do not use the laptop's own Ethernet or Wi-Fi counters: Tailscale's packets leave through them on the LAN, so they rise too and say nothing about the internet.
3. Run a fill of at least 5 GB at the source and wait for the tray to go idle.
4. After the trip, read the Tailscale counters again:

   ```powershell
   $b = Get-NetAdapterStatistics -Name Tailscale | Select-Object ReceivedBytes, SentBytes
   "{0:N2} MB received, {1:N2} MB sent" -f (($b.ReceivedBytes - $a.ReceivedBytes)/1MB), (($b.SentBytes - $a.SentBytes)/1MB)
   ```

5. Expected: Tailscale received is about the copied size (the Activity line "fill done: N carried (X MB)" gives the amount). The router's WAN counters rose by far less than that (a few MB of listings at most). If the WAN counters rose by about the copied amount, stop and report it: Ferry should have refused the copy.
6. Repeat at the destination for an unload of at least 5 GB. If the destination server is reached directly (path status "Direct on this network"), the Tailscale delta there should be near zero, and the WAN counters again should not move by the copied amount. The adapter name here is `Tailscale`; confirm with `Get-NetAdapter`.

### C. Drill (a): unplug mid-fill, then resume

1. Start a fill with a large file (a few GB) and watch it in `%LOCALAPPDATA%\Ferry\cargo\<pair>\`; a `<name>.ferry-part` file grows.
2. Unplug the network cable (or turn off Wi-Fi) while the part file is growing. Note its size.
3. Plug back in and let the next visit to the source start.
4. Expected: the same `<name>.ferry-part` continues from the noted size (it does not restart at 0), then renames to `<name>` when complete. Activity shows no error after the resume.

### D. Drill (b): flipped byte in cargo, unload at the destination

This changes only the laptop's cargo copy. The original on the source share is not touched. Use a small file (tens of MB) so the unload is quick.

1. After a fill, pick one cargo file under `%LOCALAPPDATA%\Ferry\cargo\<pair>\`. Flip one byte at offset 1000 (this works for files of any size, including over 2 GB):

   ```powershell
   $f = "$env:LOCALAPPDATA\Ferry\cargo\<pair>\<file>"
   $fs = [IO.File]::Open($f, 'Open', 'ReadWrite')
   $fs.Position = 1000; $b = $fs.ReadByte()
   $fs.Position = 1000; $fs.WriteByte($b -bxor 0xFF)
   $fs.Close()
   ```

2. Go to the destination and let the unload run.
3. Expected: no toast. Ferry sees that the laptop copy no longer matches what it carried, so retrying it can't help: it deletes the cargo file and drops it from the cargo list. Activity shows "laptop copy was damaged; will carry it again". The destination folder has no new copy of that file (its previous version, or nothing, is unchanged, and no `.ferry-part` is left behind).
4. Expected on the next visit to the source: the fill carries the file again, and the unload after that delivers it. The drill heals itself; nothing needs flipping back. (The "Copy check failed" toast is now only for a copy that arrives damaged at the destination while the laptop copy is fine, which this drill can't produce.)

### E. Drill (c): source share folder renamed

Do not do this on a folder other people use. Use a throwaway test pair.

1. On the source share, create a folder such as `ferry-drill` with a few small files. In Ferry settings add it as its own pair, then run a fill at the source and an unload at the destination so the files are mirrored at the destination.
2. At the source, rename the `ferry-drill` folder (for example to `ferry-drill-x`) before the next visit.
3. Let the fill run.
4. Expected: no file at the destination is deleted (compare the file count at the destination before and after). Activity shows an error line for that pair such as "listing failed: ...", because the folder cannot be listed. No "Deletes need your OK" toast is expected for a missing folder.
5. Variant: rename it back, then move its contents into a sibling folder (move, never delete) so the folder is empty but present. Expected: still no deletes at the destination, and Activity shows "folder ferry-drill looks empty at {Source}; deletes held" (no toast), because an empty source folder is never trusted. ({Source} is the display name you gave the source in setup, or "Source".) Move the contents back afterwards.
6. If you must use a real pair anyway, move its contents to a sibling folder and back; never delete anything. Remove the test pair from settings when done. Removing it also clears Ferry's state for it: anything still carried for that pair is deleted from the laptop and its queued deletes are dropped (Activity says "discarded N carried files of ferry-drill, ..." if there were any). Files already at the destination stay there.

### F. Drill (d): 1 GB cap, 1.38 GB zip

1. In Ferry settings set **Space for cargo** to 1 GB.
2. Put a 1.38 GB zip into a watched source folder, then let two or more fills run.
3. Expected: exactly one toast "File too big to carry" ("... is bigger than the cap, so Ferry can't carry it. Raise the cap to include it."). The second and later visits do not repeat it (the engine remembers it in `%LOCALAPPDATA%\Ferry\oversize-notified.json`). The zip is not copied. Activity shows "bigger than the cap, not carried" each visit, and that is fine.
4. To repeat the drill, delete `%LOCALAPPDATA%\Ferry\oversize-notified.json` first; otherwise the remembered file never toasts again.
5. Clean up: remove the 1.38 GB test zip from the source folder and restore the cap to its normal value.

### G. Drill (e): sneakernet with no remote addresses

This checks that Ferry mirrors a folder using only the carried destination snapshot and delete queue, with no live link to the other server. It changes your real config (remote addresses), so write the current values down first and restore them at the end. Use a throwaway test pair, not a real one.

1. In Locations, clear **Remote address (optional)** on both servers and save. Expected: the learned networks are unchanged and the flyout still shows the right place.
2. On the source share create `ferry-drill-offline` with about 20 small files (a few KB each; 20 or more, so that a single delete stays under the confirmation threshold). Add it in Folders as its own pair, using the name `ferry-drill-offline` for the destination folder too. Use a name that does not already exist at the destination; remove any old destination `ferry-drill-offline` folder from an earlier run of this drill first. Other pairs go offline as well once the remote addresses are cleared, and they take part in each trip, so either disable them in Folders for this drill or expect the counts below to include them.
3. **Source visit before any destination visit.** First check that `destination_starts_empty = false` in `%APPDATA%\Ferry\config.toml` (the first-run switch "{Destination} is empty right now" appears only in first-run setup, so if it is on, set it to false in the file and restart Ferry). At the source let Ferry run. Expected: it does not fill. Activity shows `error: Visit {Destination} once so Ferry can record what's there.` ({Destination} is the display name you gave the destination in setup, or "Destination".) With `destination_starts_empty = true` it would fill straight away instead.
4. **One destination visit.** Go to the destination and let Ferry run. Expected: Activity shows `unload done: 0 delivered (0.0 MB), 0 dropped as gone at {Source}, 0 failed, 0 deleted at {Destination}, recorded what's at {Destination} for 1 folders`. A snapshot file for the pair appears under `%LOCALAPPDATA%\Ferry\snapshots\`.
5. **One fill at the source.** Back at the source, let Ferry run. Expected: one `carried <file> (ferry-drill-offline, ...)` line per file, then `fill done: 20 carried (... MB), 0 deleted at {Destination}`. The files sit under `%LOCALAPPDATA%\Ferry\cargo\ferry-drill-offline\`.
6. **One unload.** At the destination, let Ferry run. Expected: `delivered <file> to {Destination} (ferry-drill-offline, ...)` for each file, then `unload done: 20 delivered ...`. The 20 files are at the destination with the same sizes, and cargo for the pair is empty.
7. **A delete with no link.** At the source delete one file from `ferry-drill-offline`, wait for a fill pass (expected: `fill done: 0 carried (0.0 MB), 0 deleted at {Destination}`, because Ferry cannot reach the destination from here), then go to the destination. Expected there: `deleted <file> at {Destination} (ferry-drill-offline)`, and that file is gone from the destination. Nothing else is deleted.
8. **A destination-only file (optional).** At the destination, copy a stray file into the destination `ferry-drill-offline` folder by hand. Let that destination visit finish (Ferry re-lists the destination and refreshes the snapshot), then do a pass at the source and another destination visit. Expected: the stray file is deleted at the destination on that second destination visit, because the snapshot has it and the source does not.
9. Clean up: remove the test pair in Folders (this deletes anything still carried for it and drops its queue and snapshot; files already at the destination stay, so delete that test folder at the destination yourself), delete `ferry-drill-offline` at the source, and put the remote addresses back in Locations.

### H. Auto-update from one local build to a newer one (owner runs this)

This replaces the Ferry installed for your user and restarts it, so run it when no trip matters, or on a spare Windows account or a VM. It needs NSIS to build installers locally (`winget install NSIS.NSIS`; CI builds them on Linux otherwise) and Python for a local web server. Nothing here is committed: reset the version bumps with `git checkout -- crates` at the end.

Both builds point at a local server instead of GitHub, through the build-time variable `FERRY_RELEASES_URL`, and trust a throwaway signing key instead of the real one, through `FERRY_UPDATE_KEY`. Make that key first, in Git Bash, in a folder outside the repository, for example `D:\tmp\ferry-release`:

```
openssl ecparam -name prime256v1 -genkey -noout -out test-update.pem
openssl ec -in test-update.pem -pubout -outform DER | tail -c 65 | od -An -tx1 | tr -d ' \n'
```

The second command prints the public key, 130 hex digits starting `04`.

1. **Version A (0.2.90).** Set `version = "0.2.90"` in `crates/ferry/Cargo.toml`, `crates/ferry-ui/Cargo.toml` and `crates/ferry-core/Cargo.toml`. Then in PowerShell:

   ```
   $env:FERRY_RELEASES_URL = "http://127.0.0.1:8000/latest.json"
   $env:FERRY_UPDATE_KEY = "<the 130 hex digits>"
   cargo build --release --target-dir target\update-a
   makensis -DVERSION=0.2.90 -DVERNUM=0.2.90 -DBIN="$PWD\target\update-a\release" -DOUT="$PWD\target\Ferry-Setup-0.2.90.exe" installer\ferry.nsi
   ```

2. **Version B (0.2.91).** Set the three versions to `0.2.91` and run the same four commands with `update-b` and `0.2.91` in place of `update-a` and `0.2.90`.

3. **The local "release".** Make a folder, for example `D:\tmp\ferry-release`, and copy `target\Ferry-Setup-0.2.91.exe` into it. In that folder:

   ```
   (Get-FileHash Ferry-Setup-0.2.91.exe -Algorithm SHA256).Hash.ToLower() + "  Ferry-Setup-0.2.91.exe" | Set-Content -NoNewline SHA256SUMS.txt
   ```

   Sign it, in Git Bash in the same folder: `openssl dgst -sha256 -sign test-update.pem -out SHA256SUMS.txt.sig SHA256SUMS.txt`.

   Create `latest.json` there with:

   ```
   {"tag_name": "v0.2.91", "assets": [
     {"name": "Ferry-Setup-0.2.91.exe", "browser_download_url": "http://127.0.0.1:8000/Ferry-Setup-0.2.91.exe"},
     {"name": "SHA256SUMS.txt", "browser_download_url": "http://127.0.0.1:8000/SHA256SUMS.txt"},
     {"name": "SHA256SUMS.txt.sig", "browser_download_url": "http://127.0.0.1:8000/SHA256SUMS.txt.sig"}]}
   ```

   Start the server and leave it running: `python -m http.server 8000`.

4. **Install A.** Run `target\Ferry-Setup-0.2.90.exe` and let it start Ferry. Open Settings. Expected: Updates shows "Ferry 0.2.90".

5. **Manual update.** Choose **Check for updates**. Expected: "Ferry 0.2.91 is available", with **Update to 0.2.91**, and "Update" beside Settings in the sidebar. Choose **Update to 0.2.91**. Expected, within about a minute: the window closes, the tray icon goes away and comes back, and a notification reads "Ferry updated to 0.2.91". Settings shows "Ferry 0.2.91". Activity shows `update requested`, `installing update 0.2.91`, `Ferry started` and `updated to 0.2.91`. The server's console shows the four GETs. `%LOCALAPPDATA%\Ferry\update` is empty.

6. **Automatic update.** Reinstall A (step 4), then close every Ferry window and leave it alone for about 10 minutes on an unmetered connection, not paused, with no trip running. Expected: the same restart, notification and log lines, without `update requested`.

7. **Tampered download.** Reinstall A. In the release folder, change one character of the hash in `SHA256SUMS.txt` and sign it again (the `openssl dgst` command from step 3), then choose **Check for updates** and **Update to 0.2.91**. Expected: no restart, Activity shows `update 0.2.91 failed its checksum, not installed`, the downloaded installer is gone, and `%LOCALAPPDATA%\Ferry\update\skip` contains `0.2.91`, so the automatic check won't try that version again. Delete `skip`, fix the hash and sign again afterwards.

   **Unsigned checksums.** Change the hash again but don't sign it again, delete `skip`, and repeat. Expected: no restart, Activity shows `update 0.2.91 isn't signed with Ferry's update key, not installed`, nothing was downloaded into `%LOCALAPPDATA%\Ferry\update` but `skip`, and `skip` contains `0.2.91`. Then put the right hash back, sign again, delete `skip`, rename `SHA256SUMS.txt.sig` away and repeat: the same line. Put it back and delete `skip` afterwards.

8. **Private repository.** Stop the server, start it from an empty folder so `latest.json` returns 404, and choose **Check for updates**. Expected: "Ferry 0.2.90" with "Up to date", and nothing added to Activity.

9. Put your normal Ferry back: run the latest real `Ferry-Setup-<version>.exe` from the releases page (or your previous installer), then `git checkout -- crates` and delete `target\update-a`, `target\update-b`, the two test installers and the release folder with `test-update.pem` in it.

## Update signing key

Installed copies only install a release whose `SHA256SUMS.txt.sig` is the signature of its `SHA256SUMS.txt` by a key in `UPDATE_KEY` (`crates/ferry/src/update.rs`). CI makes that file from the `UPDATE_SIGNING_KEY` Actions secret, and the release job fails if the secret is missing or its public half isn't in `UPDATE_KEY`. Until both are set up, **no tag can be released**, and copies built from this code install no updates.

### Make the key (owner, once)

In Git Bash, in a folder outside the repository:

```
openssl ecparam -name prime256v1 -genkey -noout -out ferry-update.pem
openssl ec -in ferry-update.pem -pubout -outform DER | tail -c 65 | od -An -tx1 | tr -d ' \n'
```

1. The second command prints the public key: 130 hex digits starting `04`. Put it in `crates/ferry/src/update.rs` as `UPDATE_KEY`'s default (`None => "04…",`), run `cargo test -p ferry update` (the `embedded_update_keys_are_p256_points` test checks the paste) and commit.
2. On GitHub, open the repository's **Settings > Secrets and variables > Actions**, choose **New repository secret**, name it `UPDATE_SIGNING_KEY`, and paste the whole of `ferry-update.pem`, from `-----BEGIN EC PRIVATE KEY-----` to `-----END EC PRIVATE KEY-----`. Or, with the GitHub CLI, in the folder holding the key: `gh secret set UPDATE_SIGNING_KEY --repo zaryabmustafa/ferry < ferry-update.pem`.
3. Keep `ferry-update.pem` offline, for example in a password manager or on a USB stick in a drawer, and delete every other copy. It is the only way to rotate the key without every installed copy needing a manual reinstall, and anyone holding it can ship code to every installed Ferry. Never commit it.
4. Tag a release. The release job's "Sign the checksums" step should end with `Verified OK`, and the release should list `SHA256SUMS.txt.sig`. To check one by hand: `openssl ec -in ferry-update.pem -pubout -out ferry-update.pub`, then `openssl dgst -sha256 -verify ferry-update.pub -signature SHA256SUMS.txt.sig SHA256SUMS.txt`.

### Rotate the key

Copies only fetch the latest release, so each copy has to install the one release that trusts both keys.

1. Make a new pair as above (`ferry-update-2.pem`).
2. Set `UPDATE_KEY` to the old and the new key, separated by a space. Leave the secret on the old key and release. Copies that install this release trust both keys.
3. After that release has had time to reach every copy (weeks, for a laptop that's rarely on), replace the `UPDATE_SIGNING_KEY` secret with the new `.pem`, set `UPDATE_KEY` to the new key alone, and release. A copy that missed step 2 refuses this release and every later one; it needs `Ferry-Setup-<version>.exe` run by hand from the releases page.

If the old key leaked, do both steps right away: whoever holds it can ship updates to copies that still trust it until they have installed step 3's release.

### Sign by hand instead of in CI

The secret is readable by any workflow in the repository, so someone with write access to the repository, or control of the owner's GitHub account, can still sign. To remove that, don't add the secret, and remove the "Sign the checksums" step from the release job. For each release: download `SHA256SUMS.txt` from the release, run `openssl dgst -sha256 -sign ferry-update.pem -out SHA256SUMS.txt.sig SHA256SUMS.txt` on a machine you trust, and attach `SHA256SUMS.txt.sig` to the release. Until it is attached, copies refuse that release (and skip that version), so attach it before the release is published, or publish it as a draft first.
