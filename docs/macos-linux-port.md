# Ferry on macOS and Linux: porting plan

> This is a plan for review, not a task list yet. Once the owner settles "Decisions for the owner", it becomes per-phase tasks.

**Goal:** the same source tree builds and runs Ferry on Windows, macOS and Linux. Windows behaviour and Windows code stay as they are.

**Short answer:** yes, with one large piece of work per OS. ferry-core is nearly portable. The UI is portable apart from fonts, icon glyphs and a handful of Win32 calls. The engine's `win/` folder (1.8k lines) has to be written again for each OS. The single largest risk is SMB access on Linux, because Linux has no user-level way to open `\\host\share` the way Windows does.

**Recommended order:** Phase 0 (split, Windows unchanged) → Phase 1 macOS → Phase 2 Linux. The reasons are under "Order of work".

## Global constraints (unchanged from the existing plans)

- No async runtime in the `ferry` binary. CI's "Keep the engine lean" check greps for `tokio|async-std|smol`. On Linux this also rules out anything built on `zbus` in spirit (it pulls `async-io` and `async-executor`, which are smol's parts), so every D-Bus user below goes through the `dbus` crate (libdbus, blocking) instead.
- ferry-core stays free of UI toolkits.
- Few dependencies: a new crate needs a line in this plan saying why a few lines of code can't do it.
- Copy follows DESIGN.md Voice. User-facing strings that say "Windows" get a per-OS word (see "Copy").
- `cargo test --workspace` and clippy stay clean on Windows after every task; Phase 0 adds the same on Linux for ferry-core.

## What is portable today, and what isn't

The quick survey was right about the size of the work, but a closer read finds portability bugs in code that already compiles on Unix. They must be fixed before any OS code is written:

| Where | Problem on Unix | Fix |
|---|---|---|
| `ferry-core/src/entry.rs` `mtime_of` | Returns `0` on non-Windows. Every file would look unchanged, so nothing would ever be carried. | Convert `Metadata::modified()` to FILETIME ticks (the stored format stays ticks). Export it as `entry::mtime_ticks` and use it in `trip/fill.rs` and `trip/unload.rs` instead of `MetadataExt::last_write_time`. |
| `ferry-core/src/diff.rs` `join` / `at` | Relative paths are stored with `\` separators, and `at()` does `root.join(rel)`. On Unix `a\b.zip` is one file name with a backslash in it. | Keep `\` as the stored separator (manifest, deletes and snapshots stay readable across versions) and make `at()` push components split on `\`. Same for any other `root.join(rel)` (grep `join(&c.rel` and `join(rel`). |
| `trip/job.rs` `is_disk_full`, `credentials_rejected`, the `raw_os_error() == Some(5)` checks in `job.rs` and `engine.rs` | Windows error numbers (5, 39, 67, 112). | A `sys::err` module of predicates (`is_denied`, `is_disk_full`, `is_unreachable`, `is_locked`). The Windows module keeps the exact numbers; Unix maps `EACCES`/`EPERM`, `ENOSPC`/`EDQUOT`, `EHOSTUNREACH`/`EHOSTDOWN`/`ENOENT`, `EBUSY`/`EWOULDBLOCK`. |
| `win/copy.rs` `hash_into` and the reader thread | A short read that isn't a multiple of 4096 is treated as EOF. That is true for unbuffered Windows I/O, not for a Unix network file system. A premature stop is caught by the length check, so the effect is a copy that fails every pass, not corruption. | The Unix `copy.rs` reads until `read` returns 0. |
| `ferry-ui/ui/theme.slint` | "Segoe UI Variable" and "Segoe Fluent Icons" don't exist on macOS or Linux. 54 glyph uses (`\u{E7xx}`) would render as boxes, and Segoe Fluent Icons can't be redistributed. | See "UI" below. |
| `ferry-ui/src/setup.rs`, `lib.rs` | `winit::platform::windows`, `COMPUTERNAME`, registry theme reads, `IFileOpenDialog`, `SystemParametersInfoW` work area. | See "UI" below. |

Everything else in ferry-core (config, diff, deletes, snapshot, load, location, update parsing, ipc encoding) is plain Rust and already portable. `config.rs` stores share paths with `/` (`share_path` normalises), so `config.toml` moves between OSes as is.

## Platform layout

No traits: each OS has exactly one implementation, so a trait would be an interface with one implementation per build. The contract is a set of modules with the same function signatures, checked by compiling the engine for all three targets in CI.

```
crates/ferry/src/
  win/        unchanged (Windows)
  mac/        macOS only
  linux/      Linux only
  unix/       shared by mac/ and linux/: ipc socket, lock files, statvfs, autostart files, curl, p256
  main.rs     #[cfg(windows)] mod win;  #[cfg(target_os = "macos")] mod mac;  ...
              #[cfg(windows)] use win as sys;  #[cfg(target_os = "macos")] use mac as sys;  ...
```

Engine, trip and update code say `crate::sys::net`, `sys::smb`, ... instead of `crate::win::...`. That rename (and `HWND` becoming `sys::tray::Handle`, a type alias to `HWND` on Windows) is the only edit to Windows files. Every module in `sys` exports the same items:

| Module | Items the engine uses |
|---|---|
| `sys` | `single_instance`, `attach_console`, `trim_memory`, `OS` (the word for user-facing copy) |
| `sys::tray` | `Handle`, `Icon`, `MenuCmd`, `Ev`, `create`, `set_icon`, `wake`, `set_timer`, `kill_timer`, `quit`, `run`, `set_autostart` |
| `sys::net` | `adapters`, `best_interface`, `probe_445`, `capture`, `network_name`, `metered`, `watch`/`ChangeWatch` |
| `sys::smb` | `save_credential`, `connect` (now returns the root `PathBuf` to use, see SMB) |
| `sys::copy` | `copy_hashed`, `finalize`, `hash_file` |
| `sys::power` | `keep_awake`, `background_mode`, `battery_saver_on`, `disk_free` |
| `sys::toast` | `ensure_shortcut` (no-op on Unix), `toast` |
| `sys::http` | `get`, `download`, `sha256_file`, `verify_p256` |
| `sys::err` | the error predicates above |
| `sys::install` | `installed`, `run_installer` (moved out of `update.rs` and `engine.rs`) |
| `log::stamp`, `vpn::tailscale::find_cli` | small `#[cfg]` branches in place; not worth a module |

ferry-core keeps its existing `#[cfg(windows)]` modules (`sid`, `setup_window`) and gains Unix twins next to them (`ipc` socket client, `setup_window` via lock file). The UI crate gets the same `win`/`mac`/`linux` split for its private `mod win`.

**Event loop.** Windows runs the engine inside the Win32 message loop on the main thread. On Unix the engine loop becomes a plain `mpsc` loop with `recv_timeout` to the next timer deadline (std only): `set_timer`/`kill_timer` edit a small deadline list, `wake` is a channel send. On Linux it runs on the main thread and the tray runs on its own D-Bus thread. On macOS AppKit must own the main thread, so the engine loop runs on a second thread and tray updates are posted to the main queue (`dispatch_async_f`, one `extern "C"` declaration).

## Each Windows piece and its equivalents

Risk: **L** a day or two, well-trodden; **M** needs a spike or has a known gap; **H** could change the design or a safety guarantee.

| Windows piece | Today | macOS | Linux | Crate | Risk |
|---|---|---|---|---|---|
| SMB access | UNC paths, `WNetAddConnection2W` session, OS redirector (`win/smb.rs`) | NetFS `NetFSMountURLSync` mounts `smb://<ip>/<share>` at a Ferry-owned folder, user and password passed in-process (never on a command line); unmount after the trip. All path-based code keeps working on the mount path. | No user-level mount. In-process SMB client (libsmbclient) behind a small file-system seam, see below. | none on macOS (NetFS + CoreFoundation `extern "C"`); Linux `pavao` or a thin libsmbclient FFI | mac **M**, Linux **H** |
| Credentials | Credential Manager, `Ferry/` targets only | Keychain | Secret Service (GNOME Keyring, KWallet); kernel keyutils as a fallback when no Secret Service is running | `keyring` 3 with `apple-native`, `sync-secret-service`, `crypto-rust`, `linux-native` (all on libdbus, no zbus) | **L** mac, **M** Linux |
| Gateway-MAC location | `GetAdaptersAddresses`, `ResolveIpNetEntry2`, `GetIpNetTable2`, `GetIfEntry2` hardware flag (`win/net.rs`) | `getifaddrs` (addresses, AF_LINK index and IANA `sdl_type`), `sysctl` PF_ROUTE dumps for default gateways (`RTF_GATEWAY`) and the ARP table (`RTF_LLINFO`) | `getifaddrs`, `/proc/net/route` (gateways), `/proc/net/arp` (MACs), `/sys/class/net/<if>/{type,device}` (`device` present = hardware) | `libc` only | **M** |
| Forcing an ARP lookup | `ResolveIpNetEntry2` | send one UDP datagram to the gateway's discard port, then read the ARP table again | same | none | **L** |
| Network change events | `NotifyIpInterfaceChange`, `NotifyRouteChange2`, `NotifyUnicastIpAddressChange` | PF_ROUTE socket read on a thread (RTM_NEWADDR, RTM_DELADDR, RTM_IFINFO, RTM_ADD, RTM_DELETE); the message is only "something changed" | NETLINK_ROUTE socket with link, address and route groups, read on a thread | `libc` | **L** |
| Route/adapter path check | `GetBestInterfaceEx` | connect a UDP socket to `<ip>:445` (no packet is sent), read `local_addr()`, map it to an interface with `getifaddrs`. Honours Tailscale's policy routing on Linux. | same | none | **L** |
| Adapter classification (`location::classify`) | `if_type` 53/131, Windows' hardware flag, adapter names | `utun*`, `ipsec*`, `ppp*` are VPN; `en*` with a gateway is local; ignore `awdl*`, `llw*`, `bridge*` without a gateway | ARPHRD type 65534 (tun) or no `device` is VPN | none | **M** (see "Trust switch" under Safety) |
| Network name | Network List Manager | none without Location permission (macOS 14+ redacts the Wi-Fi name). Leave empty: the UI already falls back to the router. | NetworkManager connection name over D-Bus, else empty | `dbus` (Linux) | **L** |
| Metered | `INetworkCostManager` | `false` (`ponytail:` NWPath `isExpensive` needs Network.framework blocks; add when someone updates on a hotspot) | NetworkManager `Metered` property | `dbus` | **L** |
| Tray icon and menu | `Shell_NotifyIconW`, hidden window, `TrackPopupMenu` | `NSStatusItem` + `NSMenu` | StatusNotifierItem over D-Bus. GNOME shows it only with the AppIndicator extension (Ubuntu ships it, Fedora doesn't). | mac `tray-icon` (objc2); Linux `ksni` pinned to the libdbus-based release, or ~200 lines of SNI on `dbus` | **M** |
| Tray art | `.ico` light/dark | template PNG (macOS tints it for the menu bar) | PNG pixmaps from the same art | none; `assets/brand/build.py` exports PNGs | **L** |
| Toasts | WinRT `ToastNotificationManager` + Start Menu AUMID shortcut | `UNUserNotificationCenter` (needs the signed `.app` and its bundle id); `notify-rust`'s macOS backend uses the deprecated `NSUserNotification` and is the stopgap | `org.freedesktop.Notifications` | `notify-rust` with feature `d` (libdbus) on Linux; mac via objc2 | **L** Linux, **M** mac |
| Named-pipe IPC | `\\.\pipe\ferry-<user SID>`, DACL for the user's SID, `PIPE_REJECT_REMOTE_CLIENTS`, `FIRST_PIPE_INSTANCE` | Unix socket `$TMPDIR/ferry.sock` (per-user, mode 0700 dir; under macOS's 104-byte path limit) | `$XDG_RUNTIME_DIR/ferry/ferry.sock` (0700) | std `UnixListener`/`UnixStream`; `interprocess` not needed | **L** |
| SID check (client: "the server must be us") | `GetNamedPipeServerProcessId` + token SID | `getpeereid` on the connected socket: server uid must equal `getuid()` | `SO_PEERCRED` | `libc` | **L** |
| Single instance (`Local\Ferry`) and "Settings window open" (`Local\FerrySetupWindow`) | named mutexes | `File::try_lock` on lock files in the state folder (std since Rust 1.89; toolchain is 1.99.1) | same | none | **L** |
| HTTPS GET | WinHTTP | `curl --fail --location --proto =https --proto-redir =https --max-filesize` as a child process. The update's trust comes from the signed checksums, not from TLS, so the system curl costs no security. | same; curl is on nearly every desktop | none | **L** |
| SHA-256, ECDSA P-256 | BCrypt | in-process | in-process | `sha2`, `p256` (`default-features = false`, `ecdsa`); pure Rust, small | **L** |
| Keep awake | `SetThreadExecutionState` | child `caffeinate -i -w <ferry pid>` (dies with Ferry) | child `systemd-inhibit --what=sleep:idle --who=Ferry --why="Carrying files" tail --pid=<ferry pid> -f /dev/null` (dies with Ferry); no-op when systemd isn't there | none | **L** |
| Background mode | `PROCESS_MODE_BACKGROUND_BEGIN/END` | `pthread_set_qos_class_self_np(QOS_CLASS_BACKGROUND)` on the trip thread (also throttles I/O). Check that the copy's reader thread inherits it. | `setpriority` on the trip thread's tid plus `ioprio_set` idle class. Unprivileged users can't lower nice again, so it must be per-thread on the trip worker, never process-wide. | `libc` | **M** |
| Battery Saver | `GetSystemPowerStatus` + `WM_POWERBROADCAST` | `NSProcessInfo.isLowPowerModeEnabled` | power-profiles-daemon `ActiveProfile == "power-saver"` over D-Bus | objc2 / `dbus` | **L** |
| Power change events | `RegisterPowerSettingNotification` | none in v1: read at trip start and once a minute while a trip runs (a running trip isn't idle, so the footprint rule holds) | same | none | **L** |
| Resume from sleep (`Ev::Resumed` → `NetChanged`) | `PBT_APMRESUMEAUTOMATIC` | rely on the network change that follows a wake (`ponytail:` add `NSWorkspaceDidWakeNotification` if a wired Mac stays put across sleep) | same (`ponytail:` add logind `PrepareForSleep`) | none | **L** |
| Disk free | `GetDiskFreeSpaceExW` | `statvfs` `f_bavail * f_frsize` | same | `libc` | **L** |
| Autostart | `HKCU\...\Run` | `SMAppService.mainApp` (macOS 13+), or a LaunchAgent plist in `~/Library/LaunchAgents` | `~/.config/autostart/ferry.desktop` | none | **L** |
| Install detection (`update::installed`) | `HKCU\...\Uninstall\Ferry\InstallLocation` | exe inside `/Applications/Ferry.app` or `~/Applications/Ferry.app` | `$APPIMAGE` set (AppImage self-updates); exe under `/usr` means a package manager owns it (no self-update) | none | **L** |
| Running the installer | `Ferry-Setup-x.exe /S`, then quit | see "Packaging and updates" | see "Packaging and updates" | none | **M** |
| Config and state folders | `%APPDATA%\Ferry`, `%LOCALAPPDATA%\Ferry` | `~/Library/Application Support/Ferry/` for both (not Caches: the cargo lives there) | `$XDG_CONFIG_HOME/ferry`, `$XDG_STATE_HOME/ferry`, cargo default `$XDG_DATA_HOME/ferry/cargo` | std (`HOME`, env vars); no `dirs` crate | **L** |
| Log timestamps | `GetLocalTime` | `localtime_r` | same | `libc` | **L** |
| Console attach, working-set trim | `AttachConsole`, `K32EmptyWorkingSet` | no-ops | no-ops | none | **L** |
| Tailscale CLI | `tailscale.exe` on PATH or in `%ProgramFiles%`, `CREATE_NO_WINDOW` | `tailscale` on PATH, else `/Applications/Tailscale.app/Contents/MacOS/Tailscale` | `tailscale` on PATH | none | **L** |
| Read-only share mode (`copy.rs` `FILE_SHARE_READ`) | Windows and SMB clients can read but not write the source file while it is copied; an open writer makes the open fail, so the file waits | `open(O_RDONLY \| O_SHLOCK)` on smbfs, which smbfs sends as a deny-write share mode. Needs a test against a real server. | libsmbclient `smbc_setOptionOpenShareMode(SMBC_SHAREMODE_DENY_WRITE)` on the context used for source reads | as above | **M** mac, **M** Linux |
| Unbuffered I/O and write-through | `FILE_FLAG_NO_BUFFERING`, `WRITE_THROUGH`, `SEQUENTIAL_SCAN` | `F_NOCACHE` on every source, part and read-back fd; `fsync` (SMB flush) before rename | libsmbclient has no page cache; `smbc_fsync` before rename | `libc` | **H** mac (see "What verified means") |
| Atomic replace | `MoveFileExW(REPLACE_EXISTING \| WRITE_THROUGH)` | `rename(2)` on smbfs; check it sends one SMB2 rename with ReplaceIfExists | `smbc_rename` may fall back to unlink + rename when the target exists. Check, and if so issue the SMB2 rename with ReplaceIfExists ourselves or accept and document it. | none | **M** |
| NSIS installer, Start Menu shortcut | `installer/ferry.nsi` | `.app` bundle, notarized zip (dmg optional) | AppImage, `.deb` | see Packaging | **M** |

### SMB in detail

Windows lets any program open `\\192.0.2.10\share\file` once a session exists. Ferry leans on that everywhere: `config::unc` builds roots, `entry::list_dir`, `diff`, `snapshot`, `fill` and `unload` are all `std::fs` on paths. Two properties matter for safety:

1. Bulk copies go to exactly the LAN IP of the matched fingerprint; listings and deletes may go to the remote host. So Ferry chooses the host per connection.
2. Ferry holds the credentials, so a trip can sign in without the user.

**macOS** keeps both with NetFS: mount `smb://<lan ip>/<share>` (and separately `smb://<remote host>/<share>`) under `~/Library/Application Support/Ferry/mnt/<host>/<share>`, with the Keychain password passed to `NetFSMountURLSync`. `sys::smb::connect` changes from `-> io::Result<()>` to `-> io::Result<PathBuf>` returning the root to use; the Windows version returns the same `unc(host, share)` it builds today, so `job.rs` takes roots from `connect` instead of calling `unc` itself. Before every trip the mount root is checked to be a mount point (its `st_dev` differs from its parent's), so an unmounted folder can never be mistaken for an empty server.

**Linux** can't do this without root: kernel CIFS mounts need `CAP_SYS_ADMIN` (not allowed in user namespaces), and gvfs is GNOME-only, slow through FUSE, and opens files without deny-write. "Bring your own mount" (fstab or autofs) is a trap: the mount's host is fixed, so the exact-IP rule is lost; kernel CIFS can't request deny-write; and an unmounted mount point is a local empty folder that unload would happily write into.

The recommendation is an in-process client:

- A small seam, `Share`, with the handful of operations Ferry uses: `list(dir)`, `metadata(path)`, `open_read(path)` (deny-write), `create_part(path)`, `set_len`, `set_mtime`, `rename_replace`, `remove_file`, `remove_dir`, `create_dir_all`. Free space is a laptop-side call and stays out.
- Windows and macOS implement it as `std::fs` on the root path (a few lines each; Windows code paths stay what they are today). Linux implements it with libsmbclient.
- `entry::list_dir`, `diff::DestinationView::Live`, `snapshot`, `fill`, `unload` and `copy` take the seam instead of a bare path. This is the biggest refactor in the plan (about 120 file-system call sites in `trip/` and ferry-core), and it is the main reason Linux comes second.

Alternatives considered: the pure-Rust `smb` crate (smb-rs, SMB2/3, offers single-threaded and multi-threaded modes besides tokio) would avoid the C dependency, but it is young and its share-access support needs checking; spike it next to libsmbclient and keep the one that passes the safety tests below. `pavao` wraps libsmbclient and exposes the open-share-mode option.

## UI

- **Icons:** replace Segoe Fluent Icons with a subset of Microsoft's open-source *Fluent UI System Icons* (MIT), bundled with `import "fonts/..."` in `theme.slint` and subset to the ~30 glyphs used (`assets/brand/build.py` with `pyftsubset`). The glyphs look nearly the same, so Windows can use the bundled font too and the UI stays one source; the 54 codepoints change once. If the owner would rather keep Segoe on Windows, `Theme.icons` picks per OS and the codepoints go through a small table.
- **Text font:** `apply_theme` already overrides the font on Windows 10. On macOS set it to the system font, on Linux leave it empty so fontconfig's default applies. `flyout.slint` has a baseline tuned to Segoe metrics (line 52); check it visually on both.
- **Theme and accent:** dark mode from winit's `Window::theme()` on macOS; on Linux the xdg-desktop-portal `color-scheme` setting over D-Bus (one call). No accent read on either: the existing fallback to brand amber applies. High contrast: macOS "Increase contrast" is a later task; Windows HC stays as is.
- **Flyout placement:** Windows places it above the taskbar's work area. macOS places it under the status item (tray-icon reports the icon's rect on click). On Linux X11 it goes near the click position from SNI's `Activate(x, y)`; on Wayland windows can't position themselves, so the flyout opens wherever the compositor puts it. Accept that on Wayland, or make the tray menu the primary surface there.
- **Window attributes:** `with_skip_taskbar` and `CornerPreference` are Windows-only and get `#[cfg]`; macOS hides the Dock icon with `LSUIElement` in `Info.plist` (switch to a regular activation policy while Settings is open, so it gets a menu bar and Cmd-Tab). The window icon comes from the bundle on macOS and the `.desktop` file on Linux (Wayland ignores window icons).
- **Folder picker:** macOS `osascript -e 'POSIX path of (choose folder)'`; Linux `zenity --file-selection --directory` or `kdialog --getexistingdirectory`; else the typed path the field already accepts. `rfd` would do this but pulls `zbus` (async) on Linux by default.
- **Computer name:** `COMPUTERNAME` → `scutil --get ComputerName` on macOS, `gethostname` on Linux.
- **Renderer:** keep winit + software renderer. On Retina it renders at 2x; measure flyout frame time on an older Intel Mac before considering femtovg.

### Copy

Strings that name Windows get the OS word from `sys::OS`: "Stored in Windows Credential Manager" → "Stored in your Keychain" / "Stored in your keyring"; "Start with Windows" → "Start at login" on both; "Windows Battery Saver" → "Low Power Mode" / "power saver mode"; "Windows sends traffic for 192.0.2.10 through WireGuard" → "macOS sends traffic ..." / "This laptop sends traffic ...". Examples like "D:\Ferry cargo" become "~/Ferry cargo". UNC labels in the UI (`unc_label`) read `smb://host/path` on Unix. The config key `start_with_windows` keeps its name.

## Order of work

**macOS first, then Linux.** macOS has one desktop to target (one tray API, one keychain, one notification centre), its user-level SMB mounts keep the engine path-based so most of `trip/` and ferry-core stay untouched, and a laptop that carries files between offices is more often a Mac than a Linux machine. Linux needs the `Share` seam and has the desktop split (tray on GNOME, Wayland flyout), so it benefits from a stable macOS port to compare against. The one argument for Linux first is that CI already runs on Linux; Phase 0 gets that benefit anyway.

### Phase 0: split, Windows unchanged (about a week)

1. Fix the portability bugs in the first table (`mtime_ticks`, `at()` splitting, `sys::err`). Tests for each, run on Windows.
2. `mod sys` aliasing and the `win::` → `sys::` rename; `HWND` → `sys::tray::Handle`; `CoInitializeEx` in `engine.rs` under `#[cfg(windows)]`; `connect` returns the root.
3. Stub `mac/` and `linux/` modules (`unimplemented!()` bodies) so `cargo check --target x86_64-unknown-linux-gnu` and `--target aarch64-apple-darwin` pass.
4. CI: `cargo test -p ferry-core` natively on the Linux runner; `cargo check` for the Linux and macOS targets; the lean check runs per target. Windows-only tests (`icacls`, sharing violations, Credential Manager) get `#[cfg(windows)]`.
5. UI icon font swap and per-OS copy words (this touches Windows visibly; the owner reviews screenshots).

Done when Windows builds and behaves exactly as before (installer, update, a trip on the owner's laptop) and the Unix targets compile.

### Phase 1: macOS (about three to four weeks)

1. `unix/`: IPC socket + peer uid check, lock files, `statvfs`, `localtime_r`, curl + `sha2`/`p256`, autostart file helpers.
2. `mac/net.rs`: adapters, gateways, ARP, PF_ROUTE watch, UDP best-interface. `ferry --where` output compared by hand with `netstat -rn` and `arp -an` on home and office networks.
3. `mac/smb.rs`: Keychain via `keyring`, NetFS mount and unmount, mount-point check.
4. `mac/copy.rs`: `O_SHLOCK`, `F_NOCACHE`, read to EOF, `fsync`, `rename`. Safety tests against a Samba server (see "Safety tests").
5. `mac/tray.rs`, `toast.rs`, `power.rs`; engine loop on a second thread.
6. UI: flyout placement, theme, folder picker, `LSUIElement` behaviour.
7. Packaging, signing, notarization, self-update (below).

### Phase 2: Linux (about four to five weeks)

1. Spike (3 days): libsmbclient versus smb-rs on the safety tests and on throughput over gigabit (Ferry moves 50 GB loads; the client must sustain at least what Windows does on the same link).
2. The `Share` seam through ferry-core and `trip/`, with the std implementation for Windows and macOS. Windows trip tests must still pass unchanged.
3. `linux/smb.rs` on the chosen client; `keyring` Secret Service with keyutils fallback.
4. `linux/net.rs`: `/proc`, `/sys`, netlink watch.
5. `linux/tray.rs` (SNI), notifications, power (systemd-inhibit, power-profiles-daemon).
6. Packaging (below).

## Packaging and updates

Release assets keep one `SHA256SUMS.txt` and its signature, covering every OS's files, so the update key and `checksum_for` are reused unchanged. `ferry-core::update::pick_release` learns per-OS asset names (`Ferry-Setup-<v>.exe`, `Ferry-<v>-macos.zip`, `Ferry-<v>-x86_64.AppImage`).

### macOS

- **Build:** a universal binary (`lipo` of `aarch64-apple-darwin` and `x86_64-apple-darwin`). Apple's SDK licence allows it only on Apple hardware, so this needs a Mac: GitHub's hosted macOS runners (`macos-latest`). Cross-building from the Linux runner with osxcross is technically possible but not licence-clean.
- **Bundle:** `Ferry.app` with `Info.plist` (`CFBundleIdentifier`, `LSUIElement = true`, `NSLocalNetworkUsageDescription`), the `.icns` from `build.py`, the binary in `Contents/MacOS/ferry`.
- **Signing and notarization:** an Apple Developer ID (paid membership) is required; without it Gatekeeper blocks the app and the Keychain asks again after every update because the code identity changes. `rcodesign` (apple-codesign) signs, notarizes with an App Store Connect API key and staples from any OS, so the release step can stay on the Linux runner after the Mac builds.
- **Download:** a notarized `.zip` of the stapled `.app` is enough for Gatekeeper. A `.dmg` (drag to Applications) is nicer for first install; it needs `hdiutil` on the Mac runner. Start with the zip.
- **Self-update:** download the zip, verify the signed checksum as today, unzip to the update folder, check `codesign --verify --deep --strict` and that the Team ID matches, move the old bundle aside, move the new one in, relaunch with `open -n`, and remove the old one after the new engine reports in (the existing `installing` marker and `after_restart` logic carry over). Sparkle does the same but is a large Objective-C framework.

### Linux

- **Build:** natively on the existing Ubuntu runner (`x86_64-unknown-linux-gnu`; `aarch64` later). Build against an old glibc (the oldest supported Ubuntu LTS image) so the binary runs on newer distros.
- **AppImage:** for any distro, and the one format that self-updates: download the new AppImage, verify, `chmod +x`, rename over `$APPIMAGE` (same folder, atomic), exec it. Bundling libsmbclient is heavy (it drags in many Samba libraries and is glibc-sensitive); prefer loading the system's `libsmbclient.so.0` at run time and telling the user to install it when it's missing.
- **.deb:** built with `cargo-deb`, `Depends: libsmbclient0 | libsmbclient, libdbus-1-3`. Installs a `.desktop` file and icons. Updates come through apt: a Debian repository (for example on GitHub Pages) can host it, so `apt upgrade` updates Ferry and Ferry's own updater stays off for package installs (`installed()` returns false under `/usr`). Note that apt trusts the repository's signing key, not Ferry's update key; docs/updates.md says so.
- **Later:** `.rpm` (cargo-generate-rpm) if anyone asks; Flatpak is a poor fit (sandboxed network mounts and tray).

## Safety guarantees in docs/safety.md that change on Unix

| Guarantee | macOS | Linux |
|---|---|---|
| **Verify before replace** (part file, read back, compare, rename) | Holds if the read-back reaches the server. smbfs caches file data, so the read-back must use `F_NOCACHE` on a fresh fd, and the spike must prove that a byte changed on the server after the write is seen by the read-back. If it can't be proven, the guarantee weakens to "the bytes the Mac's SMB client accepted", and safety.md must say so. | libsmbclient has no data cache, so the read-back goes over the wire. If `smbc_rename` turns out to unlink then rename, a crash between the two leaves the destination file missing (never wrong) until the next trip; either fix it with an SMB2 rename that replaces, or document it. |
| **Unstable files are skipped** ("other Windows and SMB clients can read it but not write it") | Holds through `O_SHLOCK` once the safety test passes. Reword "Windows and SMB clients" to "other computers". | Holds through libsmbclient's deny-write open mode. |
| Local laptop copies (cargo files) | Not locked against other local programs (Unix locks are advisory). The existing re-hash at unload ("If the laptop copy itself no longer matches what was carried, Ferry drops it") already covers this. | same |
| **What "verified" means** | See the first row. | Unchanged. |
| **Networks**: "Ferry asks Windows which adapter carries traffic" | Same check through the routing table; reword to "asks the operating system". | same |
| **Trust switch** ("tied to the adapter name from when you turned it on") | macOS numbers VPN adapters `utunN` and the number changes when a VPN restarts or the Mac reboots, so the switch would silently stop applying. That fails safe (copies stop) but is confusing. Bind trust to the VPN's identity instead (Tailscale by `is_tailscale`, others by their address prefix) and say so in how-it-works.md. | `wg0`, `tailscale0` are stable; unchanged. |
| **Folders and names** (`safe_name`) | Unchanged. | Unchanged; the `at()` fix keeps `\` from becoming part of a local file name. |
| **Size cap and free space** | `statvfs`; unchanged. | same |
| SMB passwords in Credential Manager (how-it-works.md "Where things live") | Keychain, with a per-app access list: stronger than Windows, where any program running as the user can read the credential. | Secret Service: any program in the user's session can read it once the keyring is unlocked, as on Windows. With only keyutils (no desktop keyring), saved passwords are lost at reboot; the UI must say "Ferry will ask again after a restart" in that case. |
| Pipe limited to the current user (not in safety.md today, in the code) | Socket in a 0700 folder plus a peer-uid check on both ends. Same guarantee. | same |
| Updates are signed | Unchanged, plus Gatekeeper and the Team ID check. | AppImage unchanged; .deb trusts the apt repository key instead (docs/updates.md). |
| Local Network permission (new) | macOS 15+ asks the user once whether Ferry may reach devices on the local network. If refused, the server looks unreachable and nothing is copied (fail-safe); the flyout must say why instead of "Can't reach Studio from here". | n/a |

### Safety tests

The Windows tests `source_cannot_be_written_during_a_copy` and `source_held_open_for_writing_is_not_copied` use a local temp folder, which can't reproduce share modes on Unix. Each Unix port adds the same two tests against a Samba server (a `samba` container on the Linux runner; a Samba or Windows share reachable from the Mac runner), plus one for the read-back (change a byte on the server between write and read-back, expect a mismatch) and one for replace atomicity.

## Dependency budget

| Crate | OS | Why not a few lines of code |
|---|---|---|
| `libc` | both | getifaddrs, sysctl, netlink, statvfs, peer credentials, localtime_r. Already in Cargo.lock through winit and slint. |
| `keyring` | both | Keychain and Secret Service protocols (the latter includes a DH session and AES); replaces `win/smb.rs`'s Credential Manager code. |
| `dbus` (libdbus) | Linux | Secret Service, notifications, SNI tray, power profiles, NetworkManager. One shared, blocking D-Bus library instead of zbus. |
| `notify-rust` (feature `d`) | Linux | Optional: notifications are one D-Bus call, so this can be skipped in favour of `dbus` directly. |
| `ksni` (libdbus release) or own SNI | Linux | StatusNotifierItem plus DBusMenu is a few hundred lines; pick after checking the pinned ksni version stays off zbus. |
| `tray-icon` (+ `objc2`, `muda`) | macOS | NSStatusItem and NSMenu from Rust; objc2 is also used for notifications, Low Power Mode and the dock policy. |
| `sha2`, `p256` | both | Replace BCrypt; pure Rust, audited, small. |
| `pavao` or libsmbclient FFI, or `smb` | Linux | An SMB2/3 client; decided by the Phase 2 spike. |

None of these pull tokio, async-std or smol; the lean check proves it per target.

## Decisions for the owner

1. **Linux SMB:** in-process client through the `Share` seam (recommended; keeps the exact-IP rule, credentials and deny-write) or "bring your own mount" (much less work, gives up the exact-IP rule and deny-write, and needs the mount-point check). Recommendation: the seam.
2. **macOS signing:** an Apple Developer ID and a Mac for the CI runner. Without both, macOS builds can be shared only as unsigned test builds that users must allow by hand.
3. **Icons:** bundled Fluent UI System Icons on every OS (one UI source, Windows looks almost the same) or keep Segoe Fluent Icons on Windows (an extra codepoint table).
4. **Wayland flyout:** accept compositor placement, or make the tray menu the main surface on Wayland.
5. **Scope of Linux packages:** AppImage and .deb only (recommended), or also .rpm.
