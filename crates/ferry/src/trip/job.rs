//! One trip on the worker thread: connect shares, map pair roots, run fill or unload.
use super::fill::run_fill;
use super::unload::run_unload;
use super::{identity, purge_unknown, unreachable, Event, FillReport, PairRoots, TripCtx, UnloadReport};
use crate::engine::Paths;
use crate::win::{net, power, smb};
use ferry_core::cargo::CargoManifest;
use ferry_core::config::{display_name, overlapping_destination, unc, unc_host, Config, Pair, Server};
use ferry_core::deletes::DeleteQueue;
use ferry_core::entry::long_path;
use ferry_core::ipc::Side;
use ferry_core::location::{locate, reason_text, BulkPath, Located, Location, PathVerdict};
use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fill,
    Unload,
}

#[derive(Debug)]
pub enum Report {
    Fill(io::Result<FillReport>),
    Unload(io::Result<UnloadReport>),
    /// Not started; a toast title and body for the user.
    Skipped { title: String, body: String },
    /// Not started: the worker's own check found this isn't the trip's place, or its bulk path
    /// is blocked; why, for Activity. `blocked_at`: the trip's own place, when the
    /// check found the laptop there and only the path is blocked.
    WrongPlace { why: String, blocked_at: Option<Location> },
}

#[derive(Debug)]
pub struct TripResult {
    pub report: Report,
    pub remote_ok: bool,
    /// The bulk path stopped being allowed during the trip, which ended it (spec 4.3).
    pub path_lost: bool,
}

/// Spec 4.3: during a trip the full path check runs again at least this often.
pub const PATH_RECHECK: Duration = Duration::from_secs(30);

/// The trip's bulk path: `quick` on every call, `full` at most every `PATH_RECHECK`; once
/// lost it stays lost.
pub(crate) struct PathGuard<Q, F> {
    quick: Q,
    full: F,
    last: Cell<Instant>,
    lost: RefCell<Option<String>>,
}

impl<Q: Fn() -> Result<(), String>, F: Fn() -> Result<(), String>> PathGuard<Q, F> {
    /// `verified`: when the trip's start check began.
    pub(crate) fn new(quick: Q, full: F, verified: Instant) -> Self {
        Self { quick, full, last: Cell::new(verified), lost: RefCell::new(None) }
    }

    pub(crate) fn ok(&self, now: Instant) -> bool {
        if self.lost.borrow().is_some() {
            return false;
        }
        let r = (self.quick)().and_then(|()| {
            if now.saturating_duration_since(self.last.get()) < PATH_RECHECK {
                return Ok(());
            }
            (self.full)().map(|()| self.last.set(now))
        });
        match r {
            Ok(()) => true,
            Err(why) => {
                *self.lost.borrow_mut() = Some(why);
                false
            }
        }
    }

    /// Why the path was lost, if it was.
    pub(crate) fn into_lost(self) -> Option<String> {
        self.lost.into_inner()
    }
}

/// The cheap check, on every call: Windows still routes `ip` through the interface the start
/// check validated.
pub(crate) fn same_route(
    route: impl Fn(IpAddr) -> Option<u32>,
    ip: IpAddr,
    start_if: Option<u32>,
) -> impl Fn() -> Result<(), String> {
    move || match route(ip) {
        Some(i) if Some(i) == start_if => Ok(()),
        _ => Err(format!("Windows now sends traffic for {ip} through a different adapter.")),
    }
}

/// Spec 4.3: bulk goes on only at the trip's side, to the server IP it was verified for
/// (`ip`, once known), over an allowed path. `Err` says why not, for the user; `place` is what
/// the user calls the trip's side.
fn bulk_here(l: Option<&Located>, want: Location, place: &str, ip: Option<IpAddr>) -> Result<IpAddr, String> {
    let l = l.ok_or("Ferry couldn't read this laptop's network adapters.")?;
    match (l.server_ip, &l.bulk) {
        (Some(srv), Some(bulk)) if l.location == want && ip.is_none_or(|ip| ip == srv) => {
            reason_text(bulk, srv).map_or(Ok(srv), Err)
        }
        _ => Err(format!("This laptop is no longer on the network at {place}.")),
    }
}

/// The VPN adapter bulk relies on trust for: one no checker vouches for, trusted on this network.
pub fn trusted_vpn(b: &BulkPath) -> Option<&str> {
    match b {
        BulkPath::ViaVpn { adapter, verdict: PathVerdict::Unknown, trusted: true } => Some(adapter),
        _ => None,
    }
}

/// Activity's note for a trip that relies on trust.
fn trust_note(l: &Located) -> Option<String> {
    let adapter = trusted_vpn(l.bulk.as_ref()?)?;
    let net = l.network_name.as_deref().unwrap_or("this network");
    Some(format!("Copied over {adapter} because you trust it on {net}."))
}

/// Current time as FILETIME ticks (100 ns since 1601).
pub fn filetime_now() -> i64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    116_444_736_000_000_000 + (d.as_nanos() / 100) as i64
}

/// The share is the first component of a pair path ("backups/vm" → "backups").
pub fn share_of(path: &str) -> &str {
    path.split(['/', '\\']).find(|s| !s.is_empty()).unwrap_or("")
}

/// A saved-credential problem (missing or rejected), as opposed to a network one.
/// `smb::connect` builds these without an OS code; WNet's own NotFound-ish codes carry one.
/// Why a share on the server the laptop is at could not be opened, as a toast.
fn share_failure(here_name: &str, user: &str, host: &str, share: &str, e: &io::Error) -> Report {
    if credentials_rejected(e) {
        return Report::Skipped {
            title: "Sign-in failed".into(),
            body: format!("{here_name} did not accept the saved sign-in. Check it in Ferry, Locations."),
        };
    }
    // ERROR_ACCESS_DENIED after a sign-in: the account (or the guest a Samba server such as Unraid
    // quietly maps a wrong password to) may not use this share.
    if e.raw_os_error() == Some(5) {
        return Report::Skipped {
            title: format!("No access to {share} on {here_name}"),
            body: format!(
                r"{here_name} refused \\{host}\{share} for {user}. Check the password in Ferry, Locations (a server like Unraid signs a wrong password in as a guest instead of refusing it), and that {user} may use this share."
            ),
        };
    }
    Report::Skipped {
        title: format!("A share on {here_name} is unreachable"),
        body: format!(r"Ferry could not open \\{host}\{share}: {e}"),
    }
}

pub fn credentials_rejected(e: &io::Error) -> bool {
    e.raw_os_error().is_none() && matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound)
}

/// Windows ERROR_DISK_FULL (112) or ERROR_HANDLE_DISK_FULL (39) in an event message.
pub fn is_disk_full(msg: &str) -> bool {
    msg.contains("(os error 112)") || msg.contains("(os error 39)")
}

/// The cargo folder: the configured one, else the default under `%LOCALAPPDATA%`.
pub fn cargo_dir(cfg: &Config, paths: &Paths) -> PathBuf {
    cfg.options.cargo_dir.clone().unwrap_or_else(|| paths.cargo.clone())
}

/// (source, destination) LAN roots for a pair. This side's comes from `lan_ip`, the address the
/// location check validated; the far side's is never copied to on this trip.
fn lan_roots(kind: Kind, cfg: &Config, lan_ip: IpAddr, p: &Pair) -> (PathBuf, PathBuf) {
    let here = unc_host(lan_ip);
    match kind {
        Kind::Fill => (unc(&here, &p.source_path), cfg.destination.lan_root(&p.destination_path)),
        Kind::Unload => (cfg.source.lan_root(&p.source_path), unc(&here, &p.destination_path)),
    }
}

fn unique_shares<'a>(pairs: &[&'a Pair], path: impl Fn(&'a Pair) -> &'a str) -> Vec<&'a str> {
    let mut v: Vec<&str> = pairs.iter().map(|p| share_of(path(p))).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The pairs a trip works on. A fill takes the enabled ones. An unload takes those too, plus
/// a disabled pair only while it still has cargo on the laptop or queued deletes; otherwise its destination share, possibly removed, would have no business in the trip.
fn select_pairs<'a>(kind: Kind, cfg_pairs: &'a [Pair], manifest: &CargoManifest, queue: &DeleteQueue) -> Vec<&'a Pair> {
    let owes = |p: &Pair| {
        manifest.items.iter().any(|i| i.pair == p.id) || queue.pairs.get(&p.id).is_some_and(|q| !q.paths.is_empty())
    };
    cfg_pairs.iter().filter(|p| p.enabled || (kind == Kind::Unload && owes(p))).collect()
}

/// Pairs kept for the trip, and pairs dropped with the error text of their unreachable share.
type Connected<'a> = (Vec<&'a Pair>, Vec<(&'a Pair, String)>);

/// Connect each distinct share of `pairs` (by `path`). A failure on a share used by an enabled
/// pair is fatal: `Err((share, error))`. A share used only by disabled pairs is not: those
/// pairs are dropped from the trip and returned with the share and error.
fn connect_shares<'a>(
    pairs: Vec<&'a Pair>,
    path: impl Fn(&'a Pair) -> &'a str + Copy,
    mut connect: impl FnMut(&str) -> io::Result<()>,
) -> Result<Connected<'a>, (String, io::Error)> {
    let mut down: Vec<(String, String)> = vec![];
    let shares: Vec<String> = unique_shares(&pairs, path).into_iter().map(str::to_owned).collect();
    for share in shares {
        if let Err(e) = connect(&share) {
            if pairs.iter().any(|p| p.enabled && share_of(path(p)) == share) {
                return Err((share, e));
            }
            down.push((share, e.to_string()));
        }
    }
    let (mut kept, mut dropped) = (vec![], vec![]);
    for p in pairs {
        match down.iter().find(|(s, _)| s == share_of(path(p))) {
            Some((_, e)) => dropped.push((p, e.clone())),
            None => kept.push(p),
        }
    }
    Ok((kept, dropped))
}

/// Run one trip. Holds background mode and keep-awake for its whole duration (both guards
/// are thread-bound, so this must run on the worker thread that drops them).
pub fn run(kind: Kind, cfg: &Config, paths: &Paths, cancel: &AtomicBool, emit: &mut dyn FnMut(Event)) -> TripResult {
    let (want, side) = if kind == Kind::Fill { (Location::Source, Side::Source) } else { (Location::Destination, Side::Destination) };
    let place = display_name(cfg, side);
    let locate_now = || net::adapters().ok().map(|a| locate(cfg, &a, &net::best_interface, &net::probe_445, &crate::vpn::check));
    // Defence in depth: bulk work only where this trip belongs, checked fresh,
    // and never over a blocked path.
    let verified = Instant::now();
    let start = locate_now();
    let lan_ip = match bulk_here(start.as_ref(), want, &place, None) {
        Ok(ip) => ip,
        Err(why) => {
            let blocked_at = start.as_ref().is_some_and(|l| l.location == want).then_some(want);
            return TripResult { report: Report::WrongPlace { why, blocked_at }, remote_ok: true, path_lost: false };
        }
    };
    // The interface the start check validated (not a fresh read, which could already differ);
    // a route that moves off it loses the path.
    let start_if = start.as_ref().and_then(|l| l.route_if);
    // Spec 4.3: checked again before each file and during copies; a lost path ends the trip.
    let trust = RefCell::new(start.as_ref().and_then(trust_note));
    let guard = PathGuard::new(
        same_route(net::best_interface, lan_ip, start_if),
        || {
            let l = locate_now();
            let r = bulk_here(l.as_ref(), want, &place, Some(lan_ip));
            if r.is_ok() {
                *trust.borrow_mut() = l.as_ref().and_then(trust_note);
            }
            r.map(drop)
        },
        verified,
    );
    let path_ok = || guard.ok(Instant::now());
    // Activity notes a trip that relied on trust, once, when its bytes first move.
    let mut noted = false;
    let mut note = |e: Event| {
        if matches!(e, Event::Progress { .. }) && !noted {
            if let Some(msg) = trust.borrow().clone() {
                noted = true;
                emit(Event::Info { pair: String::new(), msg });
            }
        }
        emit(e)
    };
    let _bg = power::background_mode();
    let _awake = power::keep_awake();
    let remote_ok = Cell::new(true);
    let report = trip(kind, cfg, paths, lan_ip, cancel, &path_ok, &remote_ok, &mut note);
    let lost = guard.into_lost();
    if let Some(msg) = &lost {
        emit(Event::Error { pair: String::new(), msg: msg.clone() });
    }
    TripResult { report, remote_ok: remote_ok.get(), path_lost: lost.is_some() }
}

#[allow(clippy::too_many_arguments)]
fn trip(
    kind: Kind,
    cfg: &Config,
    paths: &Paths,
    lan_ip: IpAddr,
    cancel: &AtomicBool,
    path_ok: &dyn Fn() -> bool,
    remote_ok: &Cell<bool>,
    emit: &mut dyn FnMut(Event),
) -> Report {
    // A disabled pair isn't filled, but what it already carries is still delivered.
    let pairs = select_pairs(kind, &cfg.pairs, &CargoManifest::load(&paths.manifest).unwrap_or_default(), &DeleteQueue::load(&paths.deletes).unwrap_or_default());
    // Setup refuses overlapping destinations; a hand-edited config may still have them, and each
    // such pair would delete the other's files. Skip them until the folder list is fixed.
    let pairs: Vec<&Pair> = pairs
        .into_iter()
        .filter(|p| match overlapping_destination(&cfg.pairs, &p.destination_path, Some(&p.id)) {
            Some(other) => {
                let msg = format!("{} overlaps the destination of {other}; skipped until one of them is changed", p.destination_path);
                emit(Event::Error { pair: p.id.clone(), msg });
                false
            }
            None => true,
        })
        .collect();
    let (source, destination) = (&cfg.source, &cfg.destination);
    // Bulk over the LAN here, metadata over the remote address there.
    let (here, there): (&Server, &Server) = match kind {
        Kind::Fill => (source, destination),
        Kind::Unload => (destination, source),
    };
    let (source_name, destination_name) = (display_name(cfg, Side::Source), display_name(cfg, Side::Destination));
    let here_name = if kind == Kind::Fill { &source_name } else { &destination_name };
    fn here_path(kind: Kind, p: &Pair) -> &str {
        if kind == Kind::Fill { &p.source_path } else { &p.destination_path }
    }

    let lan_host = unc_host(lan_ip);
    emit(Event::Step(format!("Signing in to {here_name}")));
    let connected = connect_shares(pairs, |p| here_path(kind, p), |share| smb::connect(&lan_host, share, &here.credential_target));
    let pairs = match connected {
        Ok((kept, dropped)) => {
            // Only disabled pairs rode on these shares (say, a removed destination share): skip them.
            for (p, e) in dropped {
                let msg = format!("{} is unreachable ({e}); skipped this trip", here_path(kind, p));
                emit(Event::Error { pair: p.id.clone(), msg });
            }
            kept
        }
        Err((share, e)) => return share_failure(here_name, &here.user, &lan_host, &share, &e),
    };
    // Audit S1: the network and the port answer are easy to imitate; the token this laptop left
    // on the real server isn't. Checked before anything is copied.
    if let Some(p) = pairs.first() {
        let share_root = PathBuf::from(format!(r"\\{lan_host}\{}", share_of(here_path(kind, p))));
        let side = if kind == Kind::Fill { Side::Source } else { Side::Destination };
        match identity::check(&paths.identity, side, &share_root, &identity::new_token) {
            Ok(identity::Checked::Matches) => {}
            Ok(identity::Checked::Pinned) => {
                emit(Event::Info { pair: String::new(), msg: format!("left an identity file on {here_name}, so a look-alike server can be told apart") })
            }
            Ok(identity::Checked::NotPinned(e)) => emit(Event::Info {
                pair: String::new(),
                msg: format!("couldn't leave an identity file on {here_name} ({e}), so Ferry can't tell it from a look-alike"),
            }),
            Err(why) => {
                return Report::Skipped {
                    title: format!("This may not be {here_name}"),
                    body: format!(
                        "The server at {lan_host} looks like {here_name} but {why}, so Ferry won't copy here. If you replaced or reset {here_name}, choose Remember this network for it in Ferry, Locations."
                    ),
                }
            }
        }
    }
    let there_shares = unique_shares(&pairs, |p| if kind == Kind::Fill { &p.destination_path } else { &p.source_path });
    // Shares on the far server that could not be opened over the remote address.
    let mut there_down: Vec<&str> = vec![];
    // Without a remote address there is nothing to connect: sneakernet mode (spec 4.4).
    for share in there_shares.into_iter().filter(|_| !there.remote_host.is_empty()) {
        match smb::connect(&there.remote_host, share, &there.credential_target) {
            Ok(()) => {}
            // A fill needs the destination listing. An unload only consults the source: without it,
            // everything is pushed and deletes are held.
            Err(e) if kind == Kind::Fill && credentials_rejected(&e) => {
                return Report::Skipped {
                    title: "Sign-in failed".into(),
                    body: format!("{destination_name} did not accept the saved sign-in. Check it in Ferry, Locations."),
                }
            }
            Err(_) => {
                remote_ok.set(false);
                there_down.push(share);
            }
        }
    }

    let roots: Vec<PairRoots> = pairs
        .iter()
        .map(|p| {
            let source_meta = source.remote_root(&p.source_path);
            let source_meta = match kind {
                Kind::Fill => source_meta,
                // Unload discards and deletes only against a source it can actually see.
                Kind::Unload if there_down.contains(&share_of(&p.source_path)) => None,
                Kind::Unload => source_meta.filter(|m| fs::metadata(long_path(m)).is_ok()),
            };
            // A fill never consults the source over the remote address, so a missing one is no
            // trouble; nor is a source with no remote address at all.
            if kind == Kind::Unload && source_meta.is_none() && !source.remote_host.is_empty() {
                remote_ok.set(false);
            }
            let (source_lan, destination_lan) = lan_roots(kind, cfg, lan_ip, p);
            PairRoots {
                id: p.id.clone(),
                source: source_lan,
                source_meta,
                destination: destination_lan,
                destination_meta: destination.remote_root(&p.destination_path),
                allow_empty: p.allow_empty,
            }
        })
        .collect();

    let cargo_dir = cargo_dir(cfg, paths);
    if let Err(e) = fs::create_dir_all(&cargo_dir) {
        return Report::Skipped { title: "Cargo folder unavailable".into(), body: format!("{}: {e}", cargo_dir.display()) };
    }
    let disk_free = || power::disk_free(&cargo_dir);
    let ctx = TripCtx {
        pairs: &roots,
        cargo_dir: &cargo_dir,
        manifest_path: &paths.manifest,
        deletes_path: &paths.deletes,
        snapshots: &paths.snapshots,
        source_remote_configured: !source.remote_host.is_empty(),
        destination_remote_configured: !destination.remote_host.is_empty(),
        source_name: &source_name,
        destination_name: &destination_name,
        opts: &cfg.options,
        disk_free: &disk_free,
        now: filetime_now(),
        cancel,
        path_ok,
    };
    let mut watch = |e: Event| {
        if let Event::Error { msg, .. } = &e {
            if msg.starts_with(&unreachable(&destination_name)) {
                remote_ok.set(false);
            }
            // Spec 9: destination disk full stops the unload; cargo stays on the laptop.
            if kind == Kind::Unload && is_disk_full(msg) {
                cancel.store(true, Ordering::Relaxed);
            }
        }
        emit(e)
    };
    let known: Vec<String> = cfg.pairs.iter().map(|p| p.id.clone()).collect();
    if let Err(e) = purge_unknown(&known, &cargo_dir, &paths.manifest, &paths.deletes, &paths.snapshots, &mut watch) {
        watch(Event::Error { pair: String::new(), msg: format!("clearing cargo of removed folders: {e}") });
    }
    match kind {
        Kind::Fill => Report::Fill(run_fill(&ctx, &mut watch)),
        Kind::Unload => Report::Unload(run_unload(&ctx, &mut watch)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_roots_use_the_validated_lan_ip() {
        let mut cfg = Config::default();
        cfg.source.lan_host = "office-nas".into();
        cfg.destination.lan_host = "home-nas".into();
        let p = Pair { id: "r".into(), source_path: "repositories".into(), destination_path: "backups/repos".into(), ..Default::default() };
        let ip: IpAddr = "198.51.100.10".parse().unwrap();
        assert_eq!(lan_roots(Kind::Fill, &cfg, ip, &p).0, PathBuf::from(r"\\198.51.100.10\repositories"));
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        assert_eq!(lan_roots(Kind::Unload, &cfg, ip, &p).1, PathBuf::from(r"\\10.0.0.5\backups\repos"));
    }

    const SRV: &str = "192.0.2.10";

    fn at(location: Location, bulk: BulkPath) -> Located {
        Located { location, server_ip: Some(SRV.parse().unwrap()), bulk: Some(bulk), network_name: Some("OfficeNet".into()), route_if: Some(2) }
    }

    fn via(verdict: PathVerdict, trusted: bool) -> BulkPath {
        BulkPath::ViaVpn { adapter: "WireGuard".into(), verdict, trusted }
    }

    #[test]
    fn bulk_goes_on_only_at_the_trip_side_over_an_allowed_path() {
        let ip: IpAddr = SRV.parse().unwrap();
        assert_eq!(bulk_here(Some(&at(Location::Source, BulkPath::Direct)), Location::Source, "HQ", None), Ok(ip));
        assert_eq!(bulk_here(Some(&at(Location::Source, via(PathVerdict::Unknown, true))), Location::Source, "HQ", Some(ip)), Ok(ip));
        let blocked = bulk_here(Some(&at(Location::Source, via(PathVerdict::Unknown, false))), Location::Source, "HQ", Some(ip));
        assert!(blocked.unwrap_err().contains("can't confirm that link stays on this network"));
        let relayed = bulk_here(Some(&at(Location::Source, via(PathVerdict::NotDirect("relayed".into()), true))), Location::Source, "HQ", Some(ip));
        assert!(relayed.unwrap_err().contains("relayed"), "trust never overrides NotDirect");
        let moved = bulk_here(Some(&at(Location::Destination, BulkPath::Direct)), Location::Source, "HQ", Some(ip));
        assert_eq!(moved.unwrap_err(), "This laptop is no longer on the network at HQ.");
        let other_ip = bulk_here(Some(&at(Location::Source, BulkPath::Direct)), Location::Source, "HQ", Some("192.0.2.7".parse().unwrap()));
        assert!(other_ip.is_err(), "a different server than the one verified at the start");
        let away = Located::ELSEWHERE;
        assert!(bulk_here(Some(&away), Location::Destination, "Lake house", None).is_err());
        assert!(bulk_here(None, Location::Destination, "Lake house", None).is_err());
    }

    #[test]
    fn trust_is_noted_only_when_it_carries_the_copy() {
        let note = trust_note(&at(Location::Source, via(PathVerdict::Unknown, true)));
        assert_eq!(note.as_deref(), Some("Copied over WireGuard because you trust it on OfficeNet."));
        assert_eq!(trust_note(&at(Location::Source, via(PathVerdict::Direct, true))), None, "a checker vouched for it");
        assert_eq!(trust_note(&at(Location::Source, via(PathVerdict::Unknown, false))), None);
        assert_eq!(trust_note(&at(Location::Source, BulkPath::Direct)), None);
        let unnamed = Located { network_name: None, ..at(Location::Destination, via(PathVerdict::Unknown, true)) };
        assert_eq!(trust_note(&unnamed).as_deref(), Some("Copied over WireGuard because you trust it on this network."));
    }

    #[test]
    fn path_rechecks_at_most_every_30_s_and_stays_lost() {
        let t0 = Instant::now();
        let (runs, answer) = (Cell::new(0), Cell::new(true));
        let g = PathGuard::new(|| Ok(()), || {
            runs.set(runs.get() + 1);
            if answer.get() { Ok(()) } else { Err("relayed".to_string()) }
        }, t0);
        assert!(g.ok(t0 + Duration::from_secs(29)));
        assert_eq!(runs.get(), 0, "the start check still counts");
        assert!(g.ok(t0 + Duration::from_secs(30)));
        assert_eq!(runs.get(), 1);
        assert!(g.ok(t0 + Duration::from_secs(59)));
        assert_eq!(runs.get(), 1);
        answer.set(false);
        assert!(!g.ok(t0 + Duration::from_secs(60)));
        answer.set(true);
        assert!(!g.ok(t0 + Duration::from_secs(120)), "a lost path stays lost for this trip");
        assert_eq!(runs.get(), 2);
        assert_eq!(g.into_lost().as_deref(), Some("relayed"));
    }

    #[test]
    fn route_is_checked_on_every_call() {
        let t0 = Instant::now();
        let ip: IpAddr = SRV.parse().unwrap();
        let route = Cell::new(Some(4));
        let g = PathGuard::new(same_route(|_| route.get(), ip, Some(4)), || Ok(()), t0);
        assert!(g.ok(t0));
        route.set(Some(9)); // Windows now routes the server into a VPN adapter
        assert!(!g.ok(t0), "no 30 s wait for a route change");
        assert!(g.into_lost().unwrap().contains(SRV));
        let none = PathGuard::new(same_route(|_| None, ip, Some(4)), || Ok(()), t0);
        assert!(!none.ok(t0), "no route at all");
    }

    #[test]
    fn share_is_first_component() {
        assert_eq!(share_of("backups/vm"), "backups");
        assert_eq!(share_of(r"\repositories\x"), "repositories");
        assert_eq!(share_of("docs"), "docs");
    }

    fn skipped(r: Report) -> (String, String) {
        match r {
            Report::Skipped { title, body } => (title, body),
            _ => panic!("expected Skipped"),
        }
    }

    #[test]
    fn share_failures_name_the_real_problem() {
        let rejected = io::Error::new(io::ErrorKind::PermissionDenied, "bad password");
        let (t, _) = skipped(share_failure("Archive", "alex", "192.0.2.20", "backups", &rejected));
        assert_eq!(t, "Sign-in failed");

        // Access denied (5) with a working sign-in: say so, and name the account and share.
        let denied = io::Error::from_raw_os_error(5);
        let (t, b) = skipped(share_failure("Archive", "alex", "192.0.2.20", "backups", &denied));
        assert_eq!(t, "No access to backups on Archive");
        assert!(b.contains(r"\\192.0.2.20\backups"), "{b}");
        assert!(b.contains("alex"), "{b}");
        assert!(b.contains("guest"), "{b}");

        let gone = io::Error::from_raw_os_error(67);
        let (t, _) = skipped(share_failure("Archive", "alex", "192.0.2.20", "backups", &gone));
        assert_eq!(t, "A share on Archive is unreachable");
    }

    #[test]
    fn credential_errors_are_told_apart_from_network_ones() {
        assert!(credentials_rejected(&io::Error::new(io::ErrorKind::PermissionDenied, "x")));
        assert!(credentials_rejected(&io::Error::new(io::ErrorKind::NotFound, "no saved credential")));
        assert!(!credentials_rejected(&io::Error::from_raw_os_error(53))); // bad net path
        assert!(!credentials_rejected(&io::Error::from_raw_os_error(67))); // bad net name
    }

    #[test]
    fn disk_full_is_spotted_in_messages() {
        let msg = format!("a.zip: {}", io::Error::from_raw_os_error(112));
        assert!(is_disk_full(&msg));
        assert!(!is_disk_full("a.zip: access denied (os error 5)"));
    }

    fn pair(id: &str, destination: &str, enabled: bool) -> Pair {
        Pair { id: id.into(), source_path: format!("office/{id}"), destination_path: destination.into(), enabled, ..Default::default() }
    }

    fn owing(items: &[&str], deletes: &[&str]) -> (CargoManifest, DeleteQueue) {
        let items = items.iter().map(|p| ferry_core::cargo::CargoItem { pair: (*p).into(), rel: "a".into(), size: 1, mtime: 1, hash: 1 }).collect();
        let m = CargoManifest { items, ..Default::default() };
        let mut q = DeleteQueue::default();
        for p in deletes {
            q.pairs.insert((*p).into(), ferry_core::deletes::PairDeletes { paths: vec!["x".into()], ..Default::default() });
        }
        (m, q)
    }

    #[test]
    fn unload_skips_disabled_pairs_that_owe_nothing() {
        let pairs = [pair("on", "keep/on", true), pair("idle", "gone/idle", false), pair("cargo", "gone/cargo", false), pair("del", "gone/del", false)];
        let (m, q) = owing(&["cargo"], &["del"]);
        let ids = |k| select_pairs(k, &pairs, &m, &q).iter().map(|p| p.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(Kind::Unload), ["on", "cargo", "del"]);
        assert_eq!(ids(Kind::Fill), ["on"]);
        let (m, q) = owing(&[], &[]);
        let sel = select_pairs(Kind::Unload, &pairs, &m, &q);
        assert_eq!(unique_shares(&sel, |p| &p.destination_path), ["keep"], "no connect to the idle pair's removed share");
    }

    #[test]
    fn unreachable_share_of_only_disabled_pairs_is_not_fatal() {
        let pairs = [pair("on", "keep/on", true), pair("off", "gone/off", false)];
        let (m, q) = owing(&["off"], &[]); // "off" carries cargo, so it is part of the unload
        let refs = select_pairs(Kind::Unload, &pairs, &m, &q);
        assert_eq!(refs.len(), 2);
        let down = |share: &str| if share == "gone" { Err(io::Error::from_raw_os_error(67)) } else { Ok(()) };
        let (kept, dropped) = connect_shares(refs.clone(), |p| &p.destination_path, down).unwrap();
        assert_eq!(kept.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["on"]);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].0.id, "off");
        // The same share failing for an enabled pair stays fatal.
        let pairs = [pair("on", "gone/on", true), pair("off", "gone/off", false)];
        let refs: Vec<&Pair> = pairs.iter().collect();
        assert_eq!(connect_shares(refs, |p| &p.destination_path, down).unwrap_err().0, "gone");
    }
}
