# How Ferry works

The [README](../README.md) has the short version. This page has the detail: when Ferry copies, how VPNs are handled, how it works without a remote route, where it keeps its files, and how little it costs to run.

- [When Ferry copies](#when-ferry-copies)
- [VPNs](#vpns)
- [What Ferry trusts](#what-ferry-trusts)
- [With or without a remote address](#with-or-without-a-remote-address)
- [Where things live](#where-things-live)
- [Footprint](#footprint)

## When Ferry copies

Ferry sleeps until Windows reports a network change, then works out where the laptop is. It copies file contents only when all three of these hold:

1. **The network matches.** Ferry recognises a place by the MAC address of the router (default gateway) on a local adapter, learned when you chose Remember this network. The network name from Windows is shown next to it for convenience but is not used for matching. VPN and virtual tunnel adapters don't count as the local network.
2. **The server is reachable** on its LAN address (TCP port 445).
3. **The path to the server is direct.** Ferry asks Windows which adapter carries traffic to the server's LAN address.
   - The local adapter on the matched network: copying is allowed.
   - A VPN adapter (some VPNs claim the whole local address range, so Windows sends local traffic into the tunnel): Ferry asks a path checker whether the link really stays on this network. See [VPNs](#vpns).

During a trip Ferry checks the route before each file and runs the full path check at least every 30 seconds. If the path stops being direct, the copy stops and resumes later.

When any check fails, Ferry doesn't copy file contents. It can still do metadata work, such as listing the other server over a remote address, but it never moves bulk data over a path it can tell leaves the local network.

Locations shows the path status in plain words, for example "Direct on this network", "Through Tailscale, direct on this network" or "Through WireGuard: can't confirm it stays local, so Ferry won't copy files here".

## VPNs

| Setup | What Ferry does |
|---|---|
| **No VPN** | Nothing to configure. The route to the server is the local adapter, so copying is allowed. |
| **Tailscale** | Checked. Ferry finds `tailscale.exe` (on PATH or in `%ProgramFiles%\Tailscale`), looks up which peer serves the server's address and runs `tailscale ping`. It copies only when that peer is online and reached directly at an address inside the local network; a relayed or internet path is refused. So it is safe to leave "Use Tailscale subnets" on when a server's local network is also advertised as a Tailscale subnet route. |
| **Other VPNs** (WireGuard, OpenVPN, ZeroTier and so on) | Not checked yet. If one of them carries the route to the server, Ferry recognises the place but refuses to copy, and says why in Locations and in the flyout. If the VPN doesn't capture the local address range, none of this applies. |

**The trust switch.** Each learned network has a switch, "Trust this VPN for copies", off by default. Turn it on only if you know the VPN keeps traffic to that server on the local network. Ferry can't check it, so if the VPN does send the traffic out through the internet, file copies will use your internet connection.

- It only applies when the route goes through a VPN adapter and no checker has answered.
- It is tied to the adapter name from when you turned it on.
- It never overrides a checker that says the path is relayed or over the internet.
- Each trip that relied on it is noted in Activity.

**Two limits.** If a direct VPN link degrades to a relay in the middle of a file, up to about 30 seconds of transfer can cross it before Ferry notices and stops. And turning on the trust switch for a network removes Ferry's check for that VPN on that network.

## What Ferry trusts

Ferry trusts the network it recognises. A router MAC address and an answer on port 445 are easy for someone on that network to imitate, and Ferry doesn't yet check that the server is the one it learned.

On a network you don't control, someone who copied your router's MAC address and your server's LAN address could receive the files Ferry unloads and the SMB sign-in, or hand Ferry files of their own to carry to your destination. Delete thresholds still apply.

- Pause Ferry on networks you don't trust.
- Require SMB signing on your servers if they support it.

## With or without a remote address

Each server has an optional remote address: any name or IP that reaches it from somewhere else (Tailscale, WireGuard, ZeroTier, a port forward). It is used only for listings and deletes, never for file contents.

### Both servers have one

At the source, Ferry lists the destination folders live to see what the destination is missing. At the destination, it can list the source to drop carried files that were deleted there. Deletes at the other server can be applied remotely once a folder is caught up.

### Neither has one (sneakernet)

Ferry can't see the other server from where it is, so it works from what it recorded on the last visit.

- **At the source,** it compares the source with a destination snapshot: a compact list of the destination folder (path, size, modified time) taken at the last destination visit and updated as verified files and deletes land. Files the snapshot doesn't know are carried. Files in the snapshot but gone from the source are queued for deletion at the destination.
- **At the destination,** it pushes everything carried, then applies the delete queue built at the last source visit. The destination matches the source as of that visit.
- **The first trip.** With no snapshot yet, Ferry needs to know what is at the destination. First-run setup shows a switch, "{Destination} is empty right now", where {Destination} is the name you gave the destination.
  - On: the first source visit can fill straight away.
  - Off: make the first visit to the destination, so Ferry can record the snapshot.

  The question appears only on first run; afterwards change it with `destination_starts_empty` in `config.toml`. It covers only the folder pairs that exist when you answer it: Ferry turns it off once it records the first destination snapshot.

### Only one has one (mixed)

The side with a remote address is listed live; the other works from what was recorded.

- **Only the source has one:** at the source, Ferry compares with the destination snapshot, as in sneakernet mode, so the first fill waits for a destination visit to record it (unless `destination_starts_empty` is set). At the destination, it lists the source live to drop carried files deleted there.
- **Only the destination has one:** at the source, Ferry lists the destination live. At the destination it can't see the source, so it pushes everything carried and applies the deletes queued at the last source visit without re-checking the source.

Snapshots are stored one per folder pair and read one directory at a time, so memory use stays small.

## Where things live

| What | Where |
|------|-------|
| Config (servers, folder pairs, learned networks, options) | `%APPDATA%\Ferry\config.toml` |
| Cargo (files on the laptop) | `%LOCALAPPDATA%\Ferry\cargo\<pair>\` by default; change the folder in Settings |
| Cargo list and held delete queue | `%LOCALAPPDATA%\Ferry\manifest.json`, `deletes.json` |
| Destination snapshots (sneakernet mode) | `%LOCALAPPDATA%\Ferry\snapshots\` |
| Activity log | `%LOCALAPPDATA%\Ferry\activity.log` (rotates at 1 MB to `activity.1.log`) |
| Oversize files already announced | `%LOCALAPPDATA%\Ferry\oversize-notified.json` |
| Downloaded update, until it is installed | `%LOCALAPPDATA%\Ferry\update\` |
| SMB passwords | Windows Credential Manager |

In `config.toml` the two servers are the `source` and `destination` tables, each folder pair has `source_path` and `destination_path`, and the first-run answer is `destination_starts_empty`. Older configs that use the keys `office`, `home`, `office_path`, `home_path` and `home_starts_empty` still load, and Ferry writes the new names when it saves.

Nothing is sent anywhere except to your two servers (and, if you use Tailscale, to the local `tailscale.exe`). The one other request is the daily [update check](updates.md), which sends nothing but Ferry's version in its user agent.

## Footprint

`ferry.exe` is meant to cost almost nothing when idle. It waits on network change events and makes no periodic wake-ups while idle, except for:

- the source re-check every `recheck_minutes` (default 30) while you are at the source;
- a retry 15 minutes later at the destination when a visit left work undone, or while work is waiting and copying is blocked;
- a short debounce after a network change;
- the timer for a pause you set;
- a 10-minute tick that does nothing unless a day has passed since the last update check.

Measured on 2026-10-16 on a Windows 11 development laptop, on the v1 code (release build at commit 8ada633 for the sizes, an earlier build of the same code for the memory numbers). These haven't been re-measured on the VPN-agnostic v2 code yet. Details and the checklist for re-running them are in [verification.md](verification.md).

| | v1 build |
|---|---|
| `ferry.exe` size | 1.03 MiB |
| Engine private bytes, idle | 4.17 MiB, steady over six samples |
| Engine working set, idle | 2.5 to 3.1 MB over 13.5 minutes |
| Engine CPU, idle | about 0.04% of one core |

In v1 the windows were a separate `ferry-ui.exe` (12.3 MiB). They are now built into `ferry.exe`, which is now about 13 MiB on disk. The window copy only runs while a window is open (about 4 MiB private bytes for the flyout), and the engine copy never starts the renderer, so the idle figures above shouldn't change; they haven't been re-measured on the single program yet.
